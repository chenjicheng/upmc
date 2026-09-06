use super::*;

fn fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("upmc_bridge_{name}_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn bridge_helper_launch_failure_preserves_old_executable() {
    let dir = fixture("launch_failure");
    let target = dir.join("updater.exe");
    let source = target.with_extension("exe.new");
    fs::write(&target, b"MZprevious fixture").unwrap();
    fs::write(&source, b"MZinvalid candidate fixture").unwrap();
    let bytes = fs::read(&source).unwrap();
    let expected = ExecutableIdentity {
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    };
    let result = apply_downloaded_update(&source, &target, &target, &expected, None);
    assert!(result.is_err());
    let actual = fs::read(&target).unwrap();
    fs::remove_dir_all(&dir).unwrap();
    assert_eq!(
        actual, b"MZprevious fixture",
        "failed launch destroyed the previous executable"
    );
}

#[test]
fn bridge_helper_rejects_non_executable_staging_before_replacement() {
    let dir = fixture("non_executable");
    let target = dir.join("updater.exe");
    let source = target.with_extension("exe.new");
    fs::write(&target, b"MZprevious fixture").unwrap();
    fs::write(&source, b"damaged staging").unwrap();
    let bytes = fs::read(&source).unwrap();
    let expected = ExecutableIdentity {
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    };
    let result = apply_downloaded_update(&source, &target, &target, &expected, None);
    assert!(result.is_err());
    let actual = fs::read(&target).unwrap();
    fs::remove_dir_all(&dir).unwrap();
    assert_eq!(
        actual, b"MZprevious fixture",
        "unvalidated staging replaced the executable"
    );
}

#[test]
fn bridge_helper_zero_copy_budget_performs_no_mutation() {
    let dir = fixture("zero_copy");
    let source = dir.join("source");
    let target = dir.join("target");
    fs::write(&source, b"fixture").unwrap();
    let result = copy_file_with_retry(&source, &target, 0, Duration::ZERO);
    let target_exists = target.exists();
    fs::remove_dir_all(&dir).unwrap();
    assert!(result.is_err(), "zero retry budget executed an operation");
    assert!(!target_exists);
}

#[test]
fn bridge_helper_copy_preserves_typed_permanent_error() {
    let dir = fixture("copy_error");
    let result = copy_file_with_retry(&dir.join("absent"), &dir.join("target"), 3, Duration::ZERO);
    fs::remove_dir_all(&dir).unwrap();
    assert_eq!(
        result
            .unwrap_err()
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind),
        Some(std::io::ErrorKind::NotFound)
    );
}

#[test]
fn bridge_helper_names_do_not_collide_in_one_clock_tick() {
    let names: std::collections::HashSet<_> = (0..100).map(|_| unique_helper_file_name()).collect();
    assert_eq!(names.len(), 100);
}

#[test]
fn bridge_helper_restart_forwards_selected_channel() {
    for channel in [UpdateChannel::Stable, UpdateChannel::Dev] {
        let command = restart_command(Path::new("updater.exe"), Some(channel));
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["--channel".to_owned(), channel.to_string()]);
    }
}

#[test]
fn bridge_helper_rejects_zero_expected_size_before_path_access() {
    let args: Vec<String> = [
        "updater.exe",
        "--apply-self-update",
        "--source",
        "missing.exe.new",
        "--target",
        "missing.exe",
        "--expected-size",
        "0",
        "--expected-sha256",
        &"a".repeat(64),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let error = try_run_update_helper_from(&args).unwrap_err();
    assert!(
        format!("{error:#}").contains("size must be positive"),
        "unexpected error: {error:#}"
    );
}

#[cfg(windows)]
#[test]
fn bridge_legacy_first_hop_startup_is_nonfatal_while_old_helper_is_mapped() {
    use std::process::Stdio;
    let dir = tempfile::TempDir::new().unwrap();
    let target = dir.path().join("updater.exe");
    let staging = target.with_extension("exe.new");
    let helper = dir.path().join("upmc-update-helper.exe");
    let ready = dir.path().join("ready");
    fs::write(&target, b"MZ0.4.8 candidate").unwrap();
    fs::write(&staging, b"MZold helper owned staging").unwrap();
    fs::copy(current_exe_path().unwrap(), &helper).unwrap();
    let mut child = restart_command(&helper, None)
        .args([
            "--exact",
            "selfupdate::fallback_regression_tests::windows_mapped_helper_fixture",
            "--ignored",
        ])
        .env("UPMC_TEST_SELFUPDATE_HELPER_READY", &ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let result = (|| -> Result<()> {
        for _ in 0..100 {
            if ready.try_exists()? {
                break;
            }
            ensure!(
                child.try_wait()?.is_none(),
                "legacy helper fixture exited before readiness"
            );
            thread::sleep(Duration::from_millis(10));
        }
        ensure!(
            ready.try_exists()?,
            "legacy helper fixture readiness timed out"
        );
        ensure!(
            !target.with_extension("exe.update.lock").try_exists()?,
            "old helper unexpectedly acquired new lock"
        );
        assert!(
            startup_health_ack_from(&[target.to_string_lossy().into_owned()], &target)?.is_none()
        );
        crate::observability::take_events();
        cleanup_old_exe_with(Ok(target.clone()));
        ensure!(
            staging.try_exists()? && helper.try_exists()?,
            "normal startup deleted active first-hop state"
        );
        let events = crate::observability::take_events();
        ensure!(
            events
                .iter()
                .any(|event| event["identifier"] == "selfupdate.helper_guard_failed"),
            "first-hop cleanup contention was silent"
        );
        ensure!(
            !events
                .iter()
                .any(|event| event["identifier"] == "startup.fatal"),
            "first-hop cleanup became fatal"
        );
        Ok(())
    })();
    let stopped = terminate_candidate(&mut child);
    result.unwrap();
    stopped.unwrap();
    cleanup_self_update_artifacts(&target).unwrap();
    assert!(!staging.exists() && !helper.exists());
}

#[test]
#[ignore = "isolated child fixture invoked by bridge_real_child_health_contract"]
fn bridge_candidate_health_fixture() {
    let mode = std::env::var("UPMC_TEST_HEALTH_MODE").unwrap();
    if mode == "exit" {
        return;
    }
    if mode == "healthy" {
        let path = std::env::var("UPMC_TEST_HEALTH_PATH").unwrap();
        let token = std::env::var("UPMC_TEST_HEALTH_TOKEN").unwrap();
        let executable = current_exe_path().unwrap();
        let args = vec![
            executable.to_string_lossy().into_owned(),
            SELF_UPDATE_HEALTH_ACK_ARG.to_owned(),
            path,
            SELF_UPDATE_HEALTH_TOKEN_ARG.to_owned(),
            token,
        ];
        let ack = startup_health_ack_from(&args, &executable)
            .unwrap()
            .unwrap();
        write_health_ack(&ack).unwrap();
    }
    thread::sleep(Duration::from_secs(5));
}

#[test]
fn bridge_real_child_health_contract() {
    use std::process::Stdio;
    for mode in ["healthy", "exit", "withhold"] {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("updater.exe");
        fs::copy(current_exe_path().unwrap(), &target).unwrap();
        let expected = executable_identity(&target).unwrap();
        let ack = new_startup_health_ack(&target, &expected).unwrap();
        let mut child = restart_command(&target, None)
            .args([
                "--exact",
                "selfupdate::helper_regression_tests::bridge_candidate_health_fixture",
                "--ignored",
            ])
            .env("UPMC_TEST_HEALTH_MODE", mode)
            .env("UPMC_TEST_HEALTH_PATH", &ack.path)
            .env("UPMC_TEST_HEALTH_TOKEN", &ack.token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let result = wait_for_candidate_health_with(
            &ack,
            if mode == "withhold" { 3 } else { 100 },
            Duration::from_millis(10),
            || {
                child
                    .try_wait()
                    .map(|status| status.is_some())
                    .map_err(Into::into)
            },
            read_health_ack,
            thread::sleep,
        );
        let stopped = terminate_candidate(&mut child);
        stopped.unwrap();
        assert!(child.try_wait().unwrap().is_some());
        if mode == "healthy" {
            result.unwrap();
            assert_eq!(
                fs::read_to_string(&ack.path).unwrap(),
                format!("UPMC_SELF_UPDATE_HEALTH_V1\n{}\n", ack.token)
            );
        } else {
            let error = format!("{:#}", result.unwrap_err());
            assert!(
                error.contains(if mode == "exit" {
                    "提前退出"
                } else {
                    "超时"
                }),
                "{mode}: {error}"
            );
        }
    }
}
