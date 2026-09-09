use super::*;
use self_update::http_client::{HeaderMap, HttpClient, HttpResponse};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// Once self-replace may have renamed this process's image, current_exe() is no
// longer a trustworthy installation path. Only an explicit user restart clears
// this guard. In particular, failure must not permit cleanup in the old process.
static PROCESS_MUTATED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
pub(super) fn poison_process_for_test() {
    PROCESS_MUTATED.store(true, Ordering::SeqCst);
}

pub(super) fn ensure_process_can_update() -> Result<()> {
    ensure!(
        !PROCESS_MUTATED.load(Ordering::SeqCst),
        "更新器文件已被替换；请关闭并重新启动更新器后重试"
    );
    Ok(())
}

struct BridgeRelease(self_update::Release);
impl self_update::ReleaseSource for BridgeRelease {
    fn get_releases(&self) -> self_update::Result<Vec<self_update::Release>> {
        Ok(vec![self.0.clone()])
    }
}

struct BoundedClient {
    inner: Arc<dyn HttpClient>,
    url: String,
    limit: u64,
}
struct BoundedResponse {
    inner: Box<dyn HttpResponse>,
    limit: u64,
}
impl HttpClient for BoundedClient {
    fn get(
        &self,
        url: &str,
        headers: &HeaderMap,
        timeout: Option<Duration>,
    ) -> self_update::Result<Box<dyn HttpResponse>> {
        if url != self.url {
            return Err(self_update::Error::verification_rejected(
                "unexpected bridge asset URL",
            ));
        }
        Ok(Box::new(BoundedResponse {
            inner: self.inner.get(url, headers, timeout)?,
            limit: self.limit,
        }))
    }
}
impl HttpResponse for BoundedResponse {
    fn headers(&self) -> &HeaderMap {
        self.inner.headers()
    }
    fn body(self: Box<Self>) -> Box<dyn Read> {
        // One extra byte distinguishes an exact-length response from an oversized
        // stream; checksums and the verification hooks reject that extra byte.
        Box::new(self.inner.body().take(self.limit.saturating_add(1)))
    }
}

#[cfg(test)]
fn install(info: &UpdaterVersionInfo, target: &Path, client: Arc<dyn HttpClient>) -> Result<()> {
    install_with(info, target, client, || {})
}

fn install_with(
    info: &UpdaterVersionInfo,
    target: &Path,
    client: Arc<dyn HttpClient>,
    on_install: impl Fn() + Send + Sync + 'static,
) -> Result<()> {
    let version = info.version.to_string();
    let release = self_update::Release::builder()
        .version(&version)
        .asset(self_update::ReleaseAsset::new(
            "updater.exe",
            &info.download_url,
        ))
        .build()?;
    let expected = ExecutableIdentity {
        size: info.size,
        sha256: info.sha256.clone(),
    };
    let archive_expected = expected.clone();
    let status = self_update::backends::custom::Update::configure()
        .source(BridgeRelease(release))
        .current_version(env!("CARGO_PKG_VERSION"))
        // Bridge policy already rejects downgrades and selects same-version dev
        // rebuilds. A pinned release bypasses the library's latest-version test.
        .release_tag(&version)
        .bin_name("updater")
        // Plain-file extraction copies within the download directory. Its output
        // must differ from the asset name or upstream truncates its own input.
        .bin_path_in_archive("verified-updater.exe")
        .bin_install_path(target)
        .asset_matcher(|assets| {
            assets
                .iter()
                .find(|asset| asset.name() == "updater.exe")
                .cloned()
        })
        .http_client(Arc::new(BoundedClient {
            inner: client,
            url: info.download_url.clone(),
            limit: info.size,
        }))
        .retries(config::RETRY_MAX_ATTEMPTS.saturating_sub(1) as u32)
        .retry_backoff(
            Duration::from_secs(config::RETRY_BASE_DELAY_SECS),
            Duration::from_secs(30),
        )
        .timeout(Duration::from_secs(config::DOWNLOAD_TIMEOUT_SECS))
        .no_confirm(true)
        .show_output(false)
        .show_download_progress(false)
        .verify_checksum(self_update::Checksum::Sha256(info.sha256.clone()))
        .verify_archive(move |path| {
            verify_executable_identity(path, &archive_expected)
                .map_err(|error| self_update::Error::verification_rejected(format!("{error:#}")))
        })
        .verify_binary(move |path| {
            verify_executable_identity(path, &expected)
                .map_err(|error| self_update::Error::verification_rejected(format!("{error:#}")))?;
            on_install();
            Ok(())
        })
        .build()?
        .update()?;
    ensure!(
        status.is_updated(),
        "library did not install the pinned bridge release"
    );
    Ok(())
}

pub(super) fn apply(
    info: &UpdaterVersionInfo,
    target: &Path,
    channel: UpdateChannel,
) -> Result<()> {
    ensure_process_can_update()?;
    let expected = ExecutableIdentity {
        size: info.size,
        sha256: info.sha256.clone(),
    };
    let ack = new_startup_health_ack(target, &expected)?;
    let client = Arc::new(self_update::http_client::UreqClient::from(
        bridge_http_agent(Duration::from_secs(config::DOWNLOAD_TIMEOUT_SECS)),
    ));
    let result = transaction_with(
        target,
        &expected,
        &ack,
        || {
            install_with(info, target, client, || {
                PROCESS_MUTATED.store(true, Ordering::SeqCst);
            })
        },
        |path, ack| {
            restart_command(path, Some(channel))
                .arg(SELF_UPDATE_HEALTH_ACK_ARG)
                .arg(&ack.path)
                .arg(SELF_UPDATE_HEALTH_TOKEN_ARG)
                .arg(&ack.token)
                .spawn()
        },
        wait_for_candidate_health,
        terminate_candidate,
    );
    finish_health_supervision(&ack, result)
}

fn recover(
    target: &Path,
    backup: &Path,
    previous: &ExecutableIdentity,
    error: anyhow::Error,
) -> Result<()> {
    crate::observability::event(
        "selfupdate.library_rollback",
        "restore canonical previous updater and require user restart",
        format!("{error:#}"),
        target.display(),
        backup.display(),
        "restart required",
    );
    if let Err(recovery) = restore_previous_version(target, backup, previous) {
        bail!("{error:#}\n恢复旧版更新器失败: {recovery:#}\n已保留回滚文件，请关闭更新器后恢复");
    }
    bail!("{error:#}\n已恢复旧版更新器；请关闭并重新启动更新器")
}

fn prepare_backup(target: &Path) -> Result<(PathBuf, ExecutableIdentity)> {
    let previous = executable_identity(target).context("读取旧版更新器身份失败")?;
    let backup = target.with_extension("exe.old");
    remove_stale_file(&backup)?;
    remove_stale_file(&target.with_extension("exe.old.pending"))?;
    copy_file_with_retry(
        target,
        &backup,
        REPLACEMENT_ATTEMPTS,
        REPLACEMENT_RETRY_DELAY,
    )?;
    verify_executable_identity(&backup, &previous).context("验证独立回滚副本失败")?;
    verify_executable_identity(target, &previous).context("复制期间旧版文件发生变化")?;
    Ok((backup, previous))
}

fn stop_and_recover(
    target: &Path,
    backup: &Path,
    previous: &ExecutableIdentity,
    error: anyhow::Error,
    stopped: Result<()>,
) -> Result<()> {
    if let Err(stop_error) = stopped {
        crate::observability::event(
            "selfupdate.candidate_stop_failed",
            "candidate may still run; restoration forbidden",
            format!("{error:#}; {stop_error:#}"),
            target.display(),
            backup.display(),
            "retain candidate and backup",
        );
        bail!(
            "{error:#}\n终止新版更新器失败: {stop_error:#}\n已保留新版和回滚副本；确认新版停止前禁止恢复"
        );
    }
    recover(target, backup, previous, error)
}

fn verify_installed(target: &Path, expected: &ExecutableIdentity) -> Result<()> {
    verify_executable_identity(target, expected).context("已安装更新器校验失败")?;
    Ok(())
}

fn transaction_with<Install, Launch, Supervise, Stop, Process>(
    target: &Path,
    expected: &ExecutableIdentity,
    ack: &StartupHealthAck,
    install: Install,
    launch: Launch,
    supervise: Supervise,
    stop: Stop,
) -> Result<()>
where
    Install: FnOnce() -> Result<()>,
    Launch: FnOnce(&Path, &StartupHealthAck) -> std::io::Result<Process>,
    Supervise: FnOnce(&mut Process, &StartupHealthAck) -> Result<()>,
    Stop: FnOnce(&mut Process) -> Result<()>,
{
    let (backup, previous) = prepare_backup(target)?;
    if let Err(error) = install()
        .and_then(|()| {
            verify_executable_identity(&backup, &previous).context("安装后回滚副本校验失败")
        })
        .and_then(|()| verify_installed(target, expected))
    {
        return recover(target, &backup, &previous, error);
    }
    let mut candidate = match launch(target, ack) {
        Ok(candidate) => candidate,
        Err(error) => {
            return recover(
                target,
                &backup,
                &previous,
                anyhow::Error::new(error).context("启动新版更新器失败"),
            );
        }
    };
    if let Err(error) = supervise(&mut candidate, ack) {
        return stop_and_recover(
            target,
            &backup,
            &previous,
            error.context("新版更新器健康确认失败"),
            stop(&mut candidate),
        );
    }
    // Health is committed; failure to remove an inactive backup is diagnostic,
    // never a reason to keep the old process alive beside the healthy candidate.
    crate::observability::cleanup(remove_stale_file(&backup), backup.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct BytesClient(Vec<u8>);
    struct BytesResponse {
        bytes: Vec<u8>,
        headers: HeaderMap,
    }
    impl HttpClient for BytesClient {
        fn get(
            &self,
            _: &str,
            _: &HeaderMap,
            _: Option<Duration>,
        ) -> self_update::Result<Box<dyn HttpResponse>> {
            Ok(Box::new(BytesResponse {
                bytes: self.0.clone(),
                headers: HeaderMap::new(),
            }))
        }
    }
    impl HttpResponse for BytesResponse {
        fn headers(&self) -> &HeaderMap {
            &self.headers
        }
        fn body(self: Box<Self>) -> Box<dyn Read> {
            Box::new(std::io::Cursor::new(self.bytes))
        }
    }
    fn descriptor(bytes: &[u8]) -> UpdaterVersionInfo {
        serde_json::from_value(serde_json::json!({
            "version": "99.0.0", "build_id": "a".repeat(40),
            "download_url": "https://github.com/chenjicheng/upmc/releases/download/v99.0.0/updater.exe",
            "size": bytes.len(), "sha256": format!("{:x}", Sha256::digest(bytes))
        })).unwrap()
    }
    #[test]
    fn library_installs_verified_bytes_and_leaves_no_staging_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("updater.exe");
        fs::write(&target, b"MZold fixture").unwrap();
        let bytes = b"MZnew library fixture";
        install(
            &descriptor(bytes),
            &target,
            Arc::new(BytesClient(bytes.to_vec())),
        )
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), bytes);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn library_rejects_corrupt_or_oversized_bytes_before_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("updater.exe");
        let old = b"MZold fixture";
        let good = b"MZnew library fixture";
        for bytes in [b"MZbad library fixture".to_vec(), vec![0; 100_000]] {
            fs::write(&target, old).unwrap();
            assert!(install(&descriptor(good), &target, Arc::new(BytesClient(bytes))).is_err());
            assert_eq!(fs::read(&target).unwrap(), old);
        }
    }

    #[test]
    fn install_failure_after_target_disappears_restores_independent_backup() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("updater.exe");
        let old = b"MZold fixture";
        fs::write(&target, old).unwrap();
        let expected = ExecutableIdentity {
            size: 5,
            sha256: "0".repeat(64),
        };
        let ack = new_startup_health_ack(&target, &expected).unwrap();
        let result = transaction_with(
            &target,
            &expected,
            &ack,
            || {
                fs::rename(&target, dir.path().join("library-owned-old.exe"))?;
                bail!("injected install failure after rename")
            },
            |_, _| -> std::io::Result<()> { panic!("must not launch") },
            |_, _| panic!("must not supervise"),
            |_| panic!("must not stop"),
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), old);
        assert!(format!("{:#}", result.unwrap_err()).contains("injected install failure"));
    }

    #[test]
    fn candidate_stop_failure_retains_new_target_and_verified_backup() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("updater.exe");
        let old = b"MZold fixture";
        let new = b"MZnew fixture";
        fs::write(&target, old).unwrap();
        let expected = ExecutableIdentity {
            size: new.len() as u64,
            sha256: format!("{:x}", Sha256::digest(new)),
        };
        let ack = new_startup_health_ack(&target, &expected).unwrap();
        let result = transaction_with(
            &target,
            &expected,
            &ack,
            || {
                fs::write(&target, new)?;
                Ok(())
            },
            |_, _| Ok(()),
            |_, _| bail!("health failed"),
            |_| bail!("kill denied"),
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), new);
        assert_eq!(fs::read(target.with_extension("exe.old")).unwrap(), old);
        assert!(format!("{:#}", result.unwrap_err()).contains("kill denied"));
    }

    #[test]
    fn backup_damage_during_install_blocks_candidate_launch() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("updater.exe");
        fs::write(&target, b"MZold fixture").unwrap();
        let new = b"MZnew fixture";
        let expected = ExecutableIdentity {
            size: new.len() as u64,
            sha256: format!("{:x}", Sha256::digest(new)),
        };
        let ack = new_startup_health_ack(&target, &expected).unwrap();
        let launched = std::cell::Cell::new(false);
        let result = transaction_with(
            &target,
            &expected,
            &ack,
            || {
                fs::write(&target, new)?;
                fs::write(target.with_extension("exe.old"), b"MZdamaged")?;
                Ok(())
            },
            |_, _| {
                launched.set(true);
                Ok(())
            },
            |_, _| Ok(()),
            |_| Ok(()),
        );
        assert!(result.is_err());
        assert!(!launched.get());
    }

    #[test]
    fn healthy_candidate_releases_backup_after_supervision() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("updater.exe");
        fs::write(&target, b"MZold fixture").unwrap();
        let new = b"MZnew fixture";
        let expected = ExecutableIdentity {
            size: new.len() as u64,
            sha256: format!("{:x}", Sha256::digest(new)),
        };
        let ack = new_startup_health_ack(&target, &expected).unwrap();
        transaction_with(
            &target,
            &expected,
            &ack,
            || {
                fs::write(&target, new)?;
                Ok(())
            },
            |_, _| Ok(()),
            |_, _| {
                assert!(target.with_extension("exe.old").exists());
                Ok(())
            },
            |_| panic!("healthy candidate must remain running"),
        )
        .unwrap();
        assert!(!target.with_extension("exe.old").exists());
    }

    #[test]
    fn backup_delete_failure_after_health_still_commits_success() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("updater.exe");
        fs::write(&target, b"MZold fixture").unwrap();
        let new = b"MZnew fixture";
        let expected = ExecutableIdentity {
            size: new.len() as u64,
            sha256: format!("{:x}", Sha256::digest(new)),
        };
        let ack = new_startup_health_ack(&target, &expected).unwrap();
        let locked_backup = std::cell::RefCell::new(None);
        transaction_with(
            &target,
            &expected,
            &ack,
            || {
                fs::write(&target, new)?;
                Ok(())
            },
            |_, _| Ok(()),
            |_, _| {
                *locked_backup.borrow_mut() = Some(
                    fs::OpenOptions::new()
                        .read(true)
                        .share_mode(1)
                        .open(target.with_extension("exe.old"))?,
                );
                Ok(())
            },
            |_| panic!("cleanup failure must not terminate healthy candidate"),
        )
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), new);
        assert!(target.with_extension("exe.old").exists());
    }

    #[test]
    fn native_replacement_fixture() {
        let Ok(role) = std::env::var("UPMC_NATIVE_LIBRARY_ROLE") else {
            return;
        };
        let root = PathBuf::from(std::env::var_os("UPMC_NATIVE_LIBRARY_ROOT").unwrap());
        if role == "candidate" {
            ensure_process_can_update().unwrap();
            assert!(
                acquire_update_lock(&root.join("updater.exe"), 1, Duration::ZERO).is_err(),
                "old process must retain transaction lock through supervision"
            );
            let ack = StartupHealthAck {
                path: root.join("candidate.ack"),
                token: "native-test-token".into(),
            };
            fs::write(
                root.join("candidate.started"),
                std::process::id().to_string(),
            )
            .unwrap();
            write_health_ack(&ack).unwrap();
            for _ in 0..300 {
                if root.join("candidate.stop").exists() {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
            panic!("parent did not finish native test");
        }
        let target = current_exe_path().unwrap();
        let previous = executable_identity(&target).unwrap();
        let fail_launch = role == "old-launch-failure";
        let bytes = fs::read(root.join("candidate.bytes")).unwrap();
        let info = descriptor(&bytes);
        let expected = ExecutableIdentity {
            size: info.size,
            sha256: info.sha256.clone(),
        };
        let ack = StartupHealthAck {
            path: root.join("candidate.ack"),
            token: "native-test-token".into(),
        };
        let _lock = acquire_update_lock(&target, 1, Duration::ZERO).unwrap();
        let result = transaction_with(
            &target,
            &expected,
            &ack,
            || {
                install_with(&info, &target, Arc::new(BytesClient(bytes)), || {
                    PROCESS_MUTATED.store(true, Ordering::SeqCst);
                })
            },
            |path, _| {
                if fail_launch {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "native injected candidate launch failure",
                    ));
                }
                fixture_command(path, &root, "candidate")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
            },
            wait_for_candidate_health,
            terminate_candidate,
        );
        if fail_launch {
            assert!(
                format!("{:#}", result.unwrap_err())
                    .contains("native injected candidate launch failure")
            );
            verify_executable_identity(&target, &previous).unwrap();
        } else {
            result.unwrap();
        }
        fs::write(root.join("old.completed"), b"verified").unwrap();
        let active_images: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .filter(|name| {
                name.contains(".__relocated__.")
                    || name.contains(".__selfdelete__.")
                    || name.contains(".__temp__.")
            })
            .collect();
        assert!(
            active_images
                .iter()
                .any(|name| name.contains(".__relocated__.")),
            "actual self-replace must retain the running old image until this process exits"
        );
        fs::write(
            root.join("library.active.json"),
            serde_json::to_vec(&active_images).unwrap(),
        )
        .unwrap();
        assert!(ensure_process_can_update().is_err());
        assert!(
            check_and_update(UpdateChannel::Stable, &|_| panic!(
                "poisoned retry must stop before fetching or emitting progress"
            ))
            .is_err()
        );
        assert!(cleanup_self_update_artifacts(&current_exe_path().unwrap()).is_err());
        legacy_cleanup::assert_poisoned_cleanup_retains_files();
    }

    fn fixture_command(exe: &Path, root: &Path, role: &str) -> Command {
        let mut command = restart_command(exe, None);
        command
            .args([
                "--exact",
                "selfupdate::library::tests::native_replacement_fixture",
                "--nocapture",
            ])
            .env("UPMC_NATIVE_LIBRARY_ROLE", role)
            .env("UPMC_NATIVE_LIBRARY_ROOT", root)
            // self-replace relocates the mapped image to the process temp root.
            // Isolate that root too, so the test observes every library image.
            .env("TEMP", root)
            .env("TMP", root);
        command
    }

    #[test]
    fn native_windows_library_replaces_running_exe_restarts_and_cleans_after_exit() {
        run_native_update_case("old");
    }

    #[test]
    fn native_windows_post_replace_launch_failure_restores_and_poisons_old_process() {
        run_native_update_case("old-launch-failure");
    }

    fn run_native_update_case(role: &str) {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("updater.exe");
        fs::copy(current_exe_path().unwrap(), &target).unwrap();
        let mut bytes = fs::read(&target).unwrap();
        let old_bytes = bytes.clone();
        bytes.extend_from_slice(b"UPMC native candidate distinct PE overlay");
        fs::write(dir.path().join("candidate.bytes"), &bytes).unwrap();
        let output = fixture_command(&target, dir.path(), role).output().unwrap();
        fs::write(dir.path().join("candidate.stop"), b"stop").unwrap();
        assert!(
            output.status.success(),
            "native fixture failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(dir.path().join("old.completed").exists());
        assert_eq!(dir.path().join("candidate.started").exists(), role == "old");
        assert_eq!(
            fs::read(&target).unwrap(),
            if role == "old" { bytes } else { old_bytes }
        );
        let leftovers = || {
            fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|entry| {
                    let name = entry.unwrap().file_name().to_string_lossy().to_string();
                    (name.contains(".__relocated__.")
                        || name.contains(".__selfdelete__.")
                        || name.contains(".__temp__."))
                    .then_some(name)
                })
                .collect::<Vec<_>>()
        };
        for _ in 0..100 {
            if leftovers().is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        assert!(
            leftovers().is_empty(),
            "library cleanup did not finish: {:?}",
            leftovers()
        );
        assert!(
            fs::read_dir(dir.path()).unwrap().all(|entry| !entry
                .unwrap()
                .file_type()
                .unwrap()
                .is_dir()),
            "library download staging directory must be removed"
        );
        println!(
            "native case={role}; active library images={}; library images after old exit={:?}; backup retained={}",
            fs::read_to_string(dir.path().join("library.active.json")).unwrap(),
            leftovers(),
            target.with_extension("exe.old").exists()
        );
    }
}
