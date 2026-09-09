// =====================================================// selfupdate.rs — 更新器自更新模块
// =====================================================// The HTTPS bridge validates release metadata and channel policy. The pinned
// self_update custom backend downloads, verifies and installs the exact asset;
// self-replace owns current-image replacement and its native cleanup helper.
// A verified independent backup and health supervision protect recovery.
// Legacy incoming helper arguments remain supported for already deployed builds.
// =====================================================
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{self, UpdateChannel};
const CURRENT_BUILD_ID: Option<&str> = option_env!("UPMC_BUILD_ID");

/// helper 模式参数。主程序启动时如果检测到该参数，则只执行自更新替换逻辑。
const SELF_UPDATE_HELPER_ARG: &str = "--apply-self-update";
const SELF_UPDATE_SOURCE_ARG: &str = "--source";
const SELF_UPDATE_TARGET_ARG: &str = "--target";
const SELF_UPDATE_RESTART_ARG: &str = "--restart";
const SELF_UPDATE_EXPECTED_SIZE_ARG: &str = "--expected-size";
const SELF_UPDATE_EXPECTED_SHA256_ARG: &str = "--expected-sha256";
const SELF_UPDATE_HEALTH_ACK_ARG: &str = "--self-update-health-ack";
const SELF_UPDATE_HEALTH_TOKEN_ARG: &str = "--self-update-health-token";
const SELF_UPDATE_HELPER_PREFIX: &str = "upmc-update-helper-";
const LEGACY_SELF_UPDATE_HELPER_NAME: &str = "upmc-update-helper.exe";
const HEALTH_ACK_PREFIX: &str = "upmc-self-update-health-";
const HEALTH_ACK_HEADER: &str = "UPMC_SELF_UPDATE_HEALTH_V1";
const REPLACEMENT_ATTEMPTS: usize = 30;
const REPLACEMENT_RETRY_DELAY: Duration = Duration::from_secs(1);
const HEALTH_CHECK_ATTEMPTS: usize = 300;
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExecutableIdentity {
    size: u64,
    sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupHealthAck {
    path: PathBuf,
    token: String,
}

/// 自更新检查结果
pub enum SelfUpdateResult {
    /// 无需更新，继续正常流程
    UpToDate,
    /// 已下载新版并委托 helper 替换，调用方应立即退出
    Restarting,
}

#[cfg(test)]
mod fallback_regression_tests {
    use super::*;

    #[test]
    fn audit_duplicate_health_flags_are_not_silently_overwritten() {
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("upmc.exe");
        fs::write(&target, b"MZtest").unwrap();
        let path = temp.path().join("upmc-self-update-health-test.ack");
        let args = vec![
            "upmc.exe".into(),
            SELF_UPDATE_HEALTH_ACK_ARG.into(),
            path.to_str().unwrap().into(),
            SELF_UPDATE_HEALTH_TOKEN_ARG.into(),
            "0".repeat(64),
            SELF_UPDATE_HEALTH_TOKEN_ARG.into(),
            "1".repeat(64),
        ];
        let result = startup_health_ack_from(&args, &target);
        assert!(
            result.is_err(),
            "a second token must not overwrite the first token"
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("重复"));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "subprocess fixture; invoked only by the mapped-helper test"]
    fn windows_mapped_helper_fixture() {
        let ready =
            std::env::var_os("UPMC_TEST_SELFUPDATE_HELPER_READY").expect("fixture readiness path");
        fs::write(ready, b"ready").unwrap();
        thread::sleep(Duration::from_secs(10));
    }

    #[cfg(windows)]
    #[test]
    fn mapped_windows_helper_guards_handoff_until_its_process_exits() {
        use std::os::windows::process::CommandExt;
        use std::process::Stdio;
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("upmc.exe");
        let source = target.with_extension("exe.new");
        let backup = target.with_extension("exe.old");
        let helper = temp.path().join("upmc-update-helper-mapped-test.exe");
        let ready = temp.path().join("ready");
        fs::write(&target, b"MZcandidate").unwrap();
        fs::write(&source, b"MZstaged").unwrap();
        fs::write(&backup, b"MZprevious").unwrap();
        fs::copy(std::env::current_exe().unwrap(), &helper).unwrap();
        let mut child = Command::new(&helper)
            .args([
                "--exact",
                "selfupdate::fallback_regression_tests::windows_mapped_helper_fixture",
                "--ignored",
            ])
            .env("UPMC_TEST_SELFUPDATE_HELPER_READY", &ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let result: Result<()> = (|| {
            for _ in 0..120 {
                if ready.try_exists()? {
                    break;
                }
                ensure!(
                    child.try_wait()?.is_none(),
                    "mapped helper exited before readiness"
                );
                thread::sleep(Duration::from_millis(25));
            }
            ensure!(ready.try_exists()?, "mapped helper readiness timed out");
            let _transaction = acquire_update_lock(&target, 1, Duration::ZERO)?;
            ensure!(
                remove_inactive_helpers(&target).is_err(),
                "mapped helper must block parent/helper handoff cleanup"
            );
            ensure!(
                source.try_exists()? && backup.try_exists()?,
                "helper guard mutated staging or backup"
            );
            Ok(())
        })();
        let stopped = terminate_candidate(&mut child);
        result.unwrap();
        stopped.unwrap();
        cleanup_self_update_artifacts(&target).unwrap();
        assert!(!helper.exists() && !source.exists() && !backup.exists());
    }

    #[test]
    fn retry_recovery_and_primary_success_have_exact_events_and_results() {
        crate::observability::take_events();
        let calls = std::cell::Cell::new(0);
        let sleeps = std::cell::Cell::new(0);
        copy_file_with_retry_with(
            Path::new("source"),
            Path::new("dest"),
            3,
            Duration::from_millis(10),
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    Err(sharing_error())
                } else {
                    Ok(8)
                }
            },
            |delay| {
                assert_eq!(delay, Duration::from_millis(10));
                sleeps.set(sleeps.get() + 1);
            },
        )
        .unwrap();
        assert_eq!((calls.get(), sleeps.get()), (2, 1));
        let events = crate::observability::take_events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["identifier"], "selfupdate.copy.retry");
        assert_eq!(events[1]["identifier"], "selfupdate.filesystem_recovered");
        for event in events {
            assert_eq!(event["level"], "ERROR");
            assert!(event["counter"].as_u64().unwrap() >= 1);
            for field in [
                "reason",
                "original_error",
                "primary_path",
                "fallback_path",
                "context_id",
            ] {
                assert!(!event[field].as_str().unwrap().is_empty());
            }
        }
        copy_file_with_retry_with(
            Path::new("source"),
            Path::new("dest"),
            3,
            Duration::ZERO,
            || Ok(8),
            |_| panic!("primary success must not wait"),
        )
        .unwrap();
        assert!(crate::observability::take_events().is_empty());
    }

    #[test]
    fn replacement_stops_after_backup_appears_and_retries_only_when_absent() {
        let backup = tempfile::NamedTempFile::new().unwrap();
        let calls = std::cell::Cell::new(0);
        replace_file_with_backup_with_retry_with(
            Path::new("source"),
            Path::new("target"),
            backup.path(),
            3,
            Duration::ZERO,
            || {
                calls.set(calls.get() + 1);
                Err(sharing_error().into())
            },
            || fs::symlink_metadata(backup.path()),
            |_| panic!("partial replacement must not retry"),
        )
        .unwrap_err();
        assert_eq!(calls.get(), 1);
        calls.set(0);
        replace_file_with_backup_with_retry_with(
            Path::new("source"),
            Path::new("target"),
            backup.path(),
            3,
            Duration::ZERO,
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    Err(sharing_error().into())
                } else {
                    Ok(())
                }
            },
            || Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            |_| {},
        )
        .unwrap();
        assert_eq!(calls.get(), 2);
        assert!(backup.path().exists());
    }

    #[test]
    fn restore_retries_image_lock_but_stops_permanent_failures() {
        assert!(is_transient_file_error(
            &std::io::Error::from_raw_os_error(5).into(),
            true
        ));
        assert!(!is_transient_file_error(
            &std::io::Error::from_raw_os_error(5).into(),
            false
        ));
        let calls = std::cell::Cell::new(0);
        atomic_restore_backup_with_retry_with(
            Path::new("backup"),
            Path::new("target"),
            2,
            Duration::ZERO,
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    Err(sharing_error().into())
                } else {
                    Ok(())
                }
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(calls.get(), 2);
        calls.set(0);
        atomic_restore_backup_with_retry_with(
            Path::new("backup"),
            Path::new("target"),
            2,
            Duration::ZERO,
            || {
                calls.set(calls.get() + 1);
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing backup").into())
            },
            |_| panic!("permanent restore failure must not wait"),
        )
        .unwrap_err();
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn window_watcher_success_is_silent_and_enumeration_failure_withholds_ack() {
        crate::observability::take_events();
        let temp = tempfile::TempDir::new().unwrap();
        let ack = StartupHealthAck {
            path: temp.path().join("health.ack"),
            token: "0".repeat(64),
        };
        acknowledge_health_when_window_ready_with(
            &ack,
            1,
            Duration::ZERO,
            || Err(anyhow::anyhow!("window lookup failed")),
            |_| panic!("failure must stop"),
        );
        assert!(!ack.path.exists());
        let events = crate::observability::take_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["identifier"], "selfupdate.health_window_failed");
        acknowledge_health_when_window_ready_with(
            &ack,
            1,
            Duration::ZERO,
            || Ok(true),
            |_| panic!("success must stop"),
        );
        assert!(read_health_ack(&ack).unwrap());
        assert!(crate::observability::take_events().is_empty());
    }

    #[test]
    fn cleanup_absence_is_silent_invalid_entries_and_denied_metadata_are_errors() {
        crate::observability::take_events();
        let temp = tempfile::TempDir::new().unwrap();
        remove_stale_file(&temp.path().join("absent")).unwrap();
        assert!(crate::observability::take_events().is_empty());
        assert!(remove_stale_file(temp.path()).is_err());
        let error = remove_stale_file_with(
            Path::new("denied"),
            || {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "metadata denied",
                ))
            },
            || panic!("unknown metadata cannot authorize deletion"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("metadata denied"));
    }

    #[test]
    fn lock_contention_preserves_artifacts_then_releases_for_recovery() {
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("upmc.exe");
        let backup = target.with_extension("exe.old");
        fs::write(&target, b"MZcandidate").unwrap();
        fs::write(&backup, b"MZprevious").unwrap();
        let first = acquire_update_lock(&target, 1, Duration::ZERO).unwrap();
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    assert!(acquire_update_lock(&target, 2, Duration::ZERO).is_err());
                    assert!(cleanup_self_update_artifacts(&target).is_err());
                    assert!(backup.exists());
                })
                .join()
                .unwrap();
        });
        drop(first);
        cleanup_self_update_artifacts(&target).unwrap();
        assert!(!backup.exists());
        assert!(
            target.with_extension("exe.update.lock").exists(),
            "lock inode must persist across sessions"
        );
    }

    #[test]
    fn health_supervision_preserves_probe_and_invalid_ack_errors() {
        let ack = StartupHealthAck {
            path: "health.ack".into(),
            token: "0".repeat(64),
        };
        let error = wait_for_candidate_health_with(
            &ack,
            3,
            Duration::ZERO,
            || {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "process probe denied",
                ))
            },
            |_| panic!("process probe failure must stop"),
            |_| panic!("process probe failure must stop"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("process probe denied"));
        let error = wait_for_candidate_health_with(
            &ack,
            3,
            Duration::ZERO,
            || Ok(false),
            |_| Err(anyhow::anyhow!("invalid acknowledgment")),
            |_| panic!("invalid ack must stop"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("invalid acknowledgment"));
        let checks = std::cell::Cell::new(0);
        let error = wait_for_candidate_health_with(
            &ack,
            3,
            Duration::ZERO,
            || {
                checks.set(checks.get() + 1);
                Ok(checks.get() == 2)
            },
            |_| Ok(true),
            |_| {},
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("立即退出"));
    }

    #[test]
    fn normal_arguments_and_legacy_helper_name_remain_supported() {
        assert!(!try_run_update_helper_from(&["upmc.exe".into()]).unwrap());
        assert!(
            startup_health_ack_from(&["upmc.exe".into()], Path::new("unused"))
                .unwrap()
                .is_none()
        );
        assert!(is_helper_file_name(LEGACY_SELF_UPDATE_HELPER_NAME));
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("upmc.exe");
        let source = target.with_extension("exe.new");
        let helper = temp.path().join(LEGACY_SELF_UPDATE_HELPER_NAME);
        for path in [&target, &source, &helper] {
            fs::write(path, b"MZtest").unwrap();
        }
        validate_update_helper_paths(&source, &target, &target, &helper).unwrap();
        remove_inactive_helpers(&target).unwrap();
        assert!(!helper.exists());
        assert!(
            source.exists(),
            "helper guard must not mutate staged content"
        );
    }

    #[cfg(windows)]
    #[test]
    fn audit_window_enumeration_error_is_not_window_absence() {
        let error = window_enumeration_result(
            false,
            false,
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "window enumeration denied",
            ),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("window enumeration denied"));
        assert!(
            window_enumeration_result(false, true, sharing_error()).unwrap(),
            "callback stop on a found window is successful"
        );
        assert!(!window_enumeration_result(true, false, sharing_error()).unwrap());
    }

    #[test]
    fn audit_ack_metadata_error_is_not_absence() {
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("upmc.exe");
        fs::write(&target, b"MZtest").unwrap();
        let ack = temp.path().join("upmc-self-update-health-test.ack");
        let result = validate_health_ack_path_with(&ack, &target, || {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "ack inspection denied",
            ))
        });
        assert!(
            result.is_err(),
            "inaccessible ack path must not be accepted as absent"
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("ack inspection denied"));
    }

    #[test]
    fn audit_helper_copy_failure_removes_partial_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let helper = temp.path().join("upmc-update-helper-partial.exe");
        let error = prepare_update_helper(&helper, || {
            fs::write(&helper, b"partial").unwrap();
            bail!("helper copy interrupted")
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("helper copy interrupted"));
        assert!(
            !helper.exists(),
            "failed helper preparation must clean its partial file"
        );
    }

    #[cfg(windows)]
    #[test]
    fn audit_busy_windows_helper_blocks_cleanup_before_backup_deletion() {
        use std::os::windows::fs::OpenOptionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("upmc.exe");
        let backup = target.with_extension("exe.old");
        let helper = temp.path().join("upmc-update-helper-running.exe");
        fs::write(&target, b"MZcandidate").unwrap();
        fs::write(&backup, b"MZprevious").unwrap();
        fs::write(&helper, b"MZhelper").unwrap();
        let _busy = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&helper)
            .unwrap();
        assert!(cleanup_self_update_artifacts(&target).is_err());
        assert!(
            backup.exists(),
            "helper must be checked before destroying its rollback backup"
        );
    }

    #[test]
    fn audit_helper_unknown_and_duplicate_flags_fail_explicitly() {
        let unknown: Vec<String> = ["upmc.exe", SELF_UPDATE_HELPER_ARG, "--unknown"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert!(
            format!("{:#}", try_run_update_helper_from(&unknown).unwrap_err()).contains("未知")
        );
    }

    #[test]
    fn audit_helper_duplicate_flags_fail_explicitly() {
        let duplicate: Vec<String> = [
            "upmc.exe",
            SELF_UPDATE_HELPER_ARG,
            SELF_UPDATE_SOURCE_ARG,
            "one",
            SELF_UPDATE_SOURCE_ARG,
            "two",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert!(
            format!("{:#}", try_run_update_helper_from(&duplicate).unwrap_err()).contains("重复")
        );
    }

    #[test]
    fn audit_cleanup_resolution_failure_is_observable() {
        crate::observability::take_events();
        cleanup_old_exe_with(Err(anyhow::anyhow!("current exe unavailable")));
        let events = crate::observability::take_events();
        assert_eq!(events.len(), 1);
        assert!(
            events[0]["original_error"]
                .as_str()
                .unwrap()
                .contains("current exe unavailable")
        );
    }

    #[test]
    fn audit_ack_cleanup_records_both_failures_without_changing_result() {
        crate::observability::take_events();
        let temp = tempfile::TempDir::new().unwrap();
        let ack = StartupHealthAck {
            path: temp.path().join("health.ack"),
            token: "0".repeat(64),
        };
        fs::create_dir(&ack.path).unwrap();
        fs::create_dir(ack.path.with_extension("ack.pending")).unwrap();
        finish_health_supervision(&ack, Ok(())).unwrap();
        let events = crate::observability::take_events();
        assert_eq!(events.len(), 2, "cleanup failures must not disappear");
        assert!(
            events
                .iter()
                .all(|event| event["identifier"] == "cleanup.optional" && event["level"] == "WARN")
        );
        let error = finish_health_supervision(&ack, Err(anyhow::anyhow!("primary health failure")))
            .unwrap_err();
        assert_eq!(error.to_string(), "primary health failure");
        assert_eq!(crate::observability::take_events().len(), 2);
    }

    #[test]
    fn audit_missing_health_flag_value_is_not_normal_startup() {
        for flag in [SELF_UPDATE_HEALTH_ACK_ARG, SELF_UPDATE_HEALTH_TOKEN_ARG] {
            let args = vec!["upmc.exe".into(), flag.into()];
            assert!(
                startup_health_ack_from(&args, Path::new("upmc.exe")).is_err(),
                "explicit incomplete health flag must fail: {flag}"
            );
        }
    }

    #[test]
    fn helper_omitted_restart_uses_the_same_validated_target() {
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("upmc.exe");
        let source = target.with_extension("exe.new");
        fs::write(&target, b"MZold").unwrap();
        fs::write(&source, b"MZnew").unwrap();
        let args = vec![
            "upmc.exe".into(),
            SELF_UPDATE_HELPER_ARG.into(),
            SELF_UPDATE_SOURCE_ARG.into(),
            source.to_str().unwrap().into(),
            SELF_UPDATE_TARGET_ARG.into(),
            target.to_str().unwrap().into(),
            SELF_UPDATE_EXPECTED_SIZE_ARG.into(),
            "5".into(),
            SELF_UPDATE_EXPECTED_SHA256_ARG.into(),
            "0".repeat(64),
        ];
        let error = try_run_update_helper_from(&args).unwrap_err();
        assert!(
            format!("{error:#}").contains("拒绝非自更新 helper"),
            "omitted restart must reach validated target/helper layout checks: {error:#}"
        );
        assert_eq!(fs::read(target).unwrap(), b"MZold");
    }

    #[test]
    fn audit_ack_names_and_tokens_do_not_alias_when_clock_repeats_or_precedes_epoch() {
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("upmc.exe");
        fs::write(&target, b"MZtest").unwrap();
        let expected = executable_identity(&target).unwrap();
        let before = UNIX_EPOCH - Duration::from_secs(1);
        let first = new_startup_health_ack_at(&target, &expected, before).unwrap();
        fs::write(&first.path, health_ack_contents(&first.token)).unwrap();
        let second = new_startup_health_ack_at(&target, &expected, before).unwrap();
        assert_ne!(
            first.path, second.path,
            "repeated clock must not reuse acknowledgment names"
        );
        assert_ne!(first.token, second.token);
        assert!(
            first.path.exists(),
            "new challenge must not delete another challenge"
        );
    }

    #[test]
    fn audit_cleanup_accepts_only_not_found_delete_race() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        remove_stale_file_with(
            temp.path(),
            || fs::symlink_metadata(temp.path()),
            || Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        )
        .unwrap();
        let error = remove_stale_file_with(
            temp.path(),
            || fs::symlink_metadata(temp.path()),
            || {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "cleanup denied",
                ))
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("cleanup denied"));
    }

    #[test]
    fn audit_health_watcher_failure_and_timeout_are_structured() {
        crate::observability::take_events();
        let temp = tempfile::TempDir::new().unwrap();
        let ack = StartupHealthAck {
            path: temp.path().join("missing").join("health.ack"),
            token: "0".repeat(64),
        };
        acknowledge_health_when_window_ready_with(&ack, 1, Duration::ZERO, || Ok(true), |_| {});
        let events = crate::observability::take_events();
        assert!(
            events
                .iter()
                .any(|event| event["identifier"] == "selfupdate.health_ack_failed")
        );
        acknowledge_health_when_window_ready_with(&ack, 1, Duration::ZERO, || Ok(false), |_| {});
        let events = crate::observability::take_events();
        assert!(
            events
                .iter()
                .any(|event| event["identifier"] == "selfupdate.health_window_timeout")
        );
    }

    fn sharing_error() -> std::io::Error {
        std::io::Error::from_raw_os_error(32)
    }

    #[test]
    fn audit_copy_retries_only_transient_failures_without_final_sleep() {
        crate::observability::take_events();
        let calls = std::cell::Cell::new(0);
        let sleeps = std::cell::Cell::new(0);
        let error = copy_file_with_retry_with(
            Path::new("source"),
            Path::new("dest"),
            3,
            Duration::ZERO,
            || {
                calls.set(calls.get() + 1);
                Err(sharing_error())
            },
            |_| sleeps.set(sleeps.get() + 1),
        )
        .unwrap_err();
        assert_eq!(calls.get(), 3);
        assert_eq!(sleeps.get(), 2, "never sleep after exhausting the budget");
        assert!(error.downcast_ref::<std::io::Error>().is_some());
        let events = crate::observability::take_events();
        assert_eq!(events.len(), 3);
        assert_eq!(events[2]["identifier"], "selfupdate.copy.failed");
    }

    #[test]
    fn audit_replace_metadata_failure_stops_before_second_mutation() {
        let calls = std::cell::Cell::new(0);
        let error = replace_file_with_backup_with_retry_with(
            Path::new("source"),
            Path::new("target"),
            Path::new("backup"),
            3,
            Duration::ZERO,
            || {
                calls.set(calls.get() + 1);
                Err(sharing_error().into())
            },
            || {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "backup metadata denied",
                ))
            },
            |_| {},
        )
        .unwrap_err();
        assert_eq!(
            calls.get(),
            1,
            "unknown backup state must not authorize another replacement"
        );
        assert!(format!("{error:#}").contains("backup metadata denied"));
        assert!(format!("{error:#}").contains("32"));
    }

    #[test]
    fn audit_restore_exhaustion_keeps_every_error_and_emits_events() {
        crate::observability::take_events();
        let calls = std::cell::Cell::new(0);
        let error = atomic_restore_backup_with_retry_with(
            Path::new("backup"),
            Path::new("target"),
            2,
            Duration::ZERO,
            || {
                calls.set(calls.get() + 1);
                Err(anyhow::Error::new(sharing_error()).context(format!("failure-{}", calls.get())))
            },
            |_| {},
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("failure-1"),
            "first error was discarded: {text}"
        );
        assert!(text.contains("failure-2"));
        assert!(error.downcast_ref::<std::io::Error>().is_some());
        let events = crate::observability::take_events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["identifier"], "selfupdate.restore.retry");
        assert_eq!(events[1]["identifier"], "selfupdate.restore.failed");
    }

    #[test]
    fn audit_replace_permanent_error_is_not_retried() {
        let calls = std::cell::Cell::new(0);
        let error = replace_file_with_backup_with_retry_with(
            Path::new("source"),
            Path::new("target"),
            Path::new("backup"),
            3,
            Duration::ZERO,
            || {
                calls.set(calls.get() + 1);
                Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid replace").into())
            },
            || Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            |_| {},
        )
        .unwrap_err();
        assert_eq!(calls.get(), 1);
        assert!(format!("{error:#}").contains("invalid replace"));
    }

    #[test]
    fn audit_helper_names_are_unique_within_one_clock_tick() {
        let names: std::collections::HashSet<_> =
            (0..2000).map(|_| unique_helper_file_name()).collect();
        assert_eq!(
            names.len(),
            2000,
            "clock resolution must not alias helper files"
        );
    }

    #[test]
    fn audit_copy_zero_budget_has_no_side_effect() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("source");
        let dest = temp.path().join("dest");
        fs::write(&source, b"content").unwrap();
        assert!(copy_file_with_retry(&source, &dest, 0, Duration::ZERO).is_err());
        assert!(!dest.exists());
    }

    #[test]
    fn audit_replace_zero_budget_has_no_side_effect() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        let backup = temp.path().join("backup");
        fs::write(&source, b"new").unwrap();
        fs::write(&target, b"old").unwrap();
        assert!(
            replace_file_with_backup_with_retry(&source, &target, &backup, 0, Duration::ZERO)
                .is_err()
        );
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(!backup.exists());
    }

    #[test]
    fn audit_restore_zero_budget_has_no_side_effect() {
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("target");
        let backup = temp.path().join("backup");
        fs::write(&target, b"new").unwrap();
        fs::write(&backup, b"old").unwrap();
        assert!(atomic_restore_backup_with_retry(&backup, &target, 0, Duration::ZERO).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert!(backup.exists());
    }

    #[test]
    fn audit_health_zero_budget_never_accepts_ack() {
        let ack = StartupHealthAck {
            path: "unused.ack".into(),
            token: "0".repeat(64),
        };
        let checks = std::cell::Cell::new(0);
        let result = wait_for_candidate_health_with(
            &ack,
            0,
            Duration::ZERO,
            || {
                checks.set(checks.get() + 1);
                Ok(false)
            },
            |_| Ok(true),
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(checks.get(), 0);
    }

    #[test]
    fn audit_copy_failure_preserves_typed_error_and_records_stop() {
        crate::observability::take_events();
        let temp = tempfile::TempDir::new().unwrap();
        let error = copy_file_with_retry(
            &temp.path().join("missing"),
            &temp.path().join("dest"),
            2,
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(
            error.downcast_ref::<std::io::Error>().is_some(),
            "typed filesystem cause was discarded: {error:#}"
        );
        let events = crate::observability::take_events();
        assert_eq!(
            events.len(),
            1,
            "permanent failure must stop immediately and be observable"
        );
        assert_eq!(events[0]["identifier"], "selfupdate.copy.failed");
    }

    #[test]
    fn audit_restore_failure_preserves_target_validation_error() {
        crate::observability::take_events();
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("upmc.exe");
        let backup = temp.path().join("upmc.exe.old");
        fs::write(&target, b"invalid candidate").unwrap();
        let error = restore_previous_version(
            &target,
            &backup,
            &ExecutableIdentity {
                size: 10,
                sha256: "0".repeat(64),
            },
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("PE"),
            "target validation cause was lost: {error:#}"
        );
        let events = crate::observability::take_events();
        assert!(
            events
                .iter()
                .any(|event| event["identifier"] == "selfupdate.restore_required")
        );
    }

    #[test]
    fn audit_rollback_restart_failure_is_structured() {
        crate::observability::take_events();
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("upmc.exe");
        fs::write(&target, b"MZold").unwrap();
        let previous = executable_identity(&target).unwrap();
        let error = fail_update_and_restart_previous(
            "primary failure".into(),
            &target,
            &target.with_extension("exe.old"),
            &previous,
            &target,
            &mut |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "restart denied",
                ))
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("primary failure"));
        assert!(format!("{error:#}").contains("restart denied"));
        assert!(
            crate::observability::take_events()
                .iter()
                .any(|event| event["identifier"] == "selfupdate.restart_failed")
        );
    }

    #[test]
    fn audit_cleanup_does_not_delete_live_transaction_artifacts() {
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("upmc.exe");
        let backup = target.with_extension("exe.old");
        fs::write(&target, b"MZcandidate").unwrap();
        fs::write(&backup, b"MZprevious").unwrap();
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(target.with_extension("exe.update.lock"))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        let result = cleanup_self_update_artifacts(&target);
        assert!(result.is_err(), "live transaction must block cleanup");
        assert_eq!(fs::read(&backup).unwrap(), b"MZprevious");
    }

    #[test]
    fn fallback_regression_live_unhealthy_candidate_blocks_rollback_and_relaunch() {
        let temp = tempfile::TempDir::new().unwrap();
        let target = temp.path().join("upmc.exe");
        let source = temp.path().join("upmc.exe.new");
        fs::write(&target, b"MZold updater").unwrap();
        fs::write(&source, b"MZnew candidate").unwrap();
        let expected = executable_identity(&source).unwrap();
        let ack = new_startup_health_ack(&target, &expected).unwrap();
        let relaunched = std::cell::Cell::new(false);
        let result = apply_downloaded_update_with(
            &source,
            &target,
            &target,
            &expected,
            &ack,
            UpdateHooks {
                replace: |source: &Path, target: &Path, backup: &Path| {
                    replace_file_with_backup_with_retry(source, target, backup, 1, Duration::ZERO)
                },
                launch_candidate: |_path: &Path, _ack: &StartupHealthAck| Ok(()),
                supervise: |_candidate: &mut (), _ack: &StartupHealthAck| {
                    Err(anyhow::anyhow!("health deadline expired"))
                },
                stop_candidate: |_candidate: &mut ()| {
                    Err(anyhow::anyhow!("candidate termination denied"))
                },
                launch_previous: |_path: &Path| {
                    relaunched.set(true);
                    Ok(())
                },
            },
        );
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("health deadline expired"));
        assert!(error.contains("candidate termination denied"));
        assert!(
            !relaunched.get(),
            "an unconfirmed running candidate must prevent a second updater launch"
        );
        assert_eq!(fs::read(&target).unwrap(), b"MZnew candidate");
        assert_eq!(
            fs::read(target.with_extension("exe.old")).unwrap(),
            b"MZold updater"
        );
    }
}

/// 获取当前 exe 的路径
fn current_exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("无法获取当前 exe 路径")
}

/// 如果当前进程是自更新 helper，则执行替换流程并返回 true。
///
/// 该函数必须在 main() 最开始调用，避免 helper 初始化 GUI 或执行正常更新流程。
pub fn try_run_update_helper_from_args() -> Result<bool> {
    let args: Vec<String> = std::env::args().collect();
    try_run_update_helper_from(&args)
}

fn try_run_update_helper_from(args: &[String]) -> Result<bool> {
    if args.get(1).map(String::as_str) != Some(SELF_UPDATE_HELPER_ARG) {
        return Ok(false);
    }

    let mut source: Option<PathBuf> = None;
    let mut target: Option<PathBuf> = None;
    let mut restart: Option<PathBuf> = None;
    let mut channel: Option<UpdateChannel> = None;
    let mut expected_size: Option<String> = None;
    let mut expected_sha256: Option<String> = None;

    let mut i = 2;
    let mut seen = std::collections::HashSet::new();
    while i < args.len() {
        let flag = args[i].as_str();
        ensure!(
            matches!(
                flag,
                SELF_UPDATE_SOURCE_ARG
                    | SELF_UPDATE_TARGET_ARG
                    | SELF_UPDATE_RESTART_ARG
                    | SELF_UPDATE_EXPECTED_SIZE_ARG
                    | SELF_UPDATE_EXPECTED_SHA256_ARG
                    | "--channel"
            ),
            "自更新 helper 未知参数: {flag}"
        );
        ensure!(seen.insert(flag), "自更新 helper 参数重复: {flag}");
        let value = args
            .get(i + 1)
            .filter(|value| !value.is_empty() && !value.starts_with("--"))
            .with_context(|| format!("自更新 helper 参数缺少值: {flag}"))?;
        match args[i].as_str() {
            SELF_UPDATE_SOURCE_ARG => {
                source = Some(PathBuf::from(value));
                i += 2;
            }
            SELF_UPDATE_TARGET_ARG => {
                target = Some(PathBuf::from(value));
                i += 2;
            }
            SELF_UPDATE_RESTART_ARG => {
                restart = Some(PathBuf::from(value));
                i += 2;
            }
            "--channel" => {
                channel = Some(match value.as_str() {
                    "stable" => UpdateChannel::Stable,
                    "dev" => UpdateChannel::Dev,
                    _ => bail!("invalid self-update channel: {value}"),
                });
                i += 2;
            }
            SELF_UPDATE_EXPECTED_SIZE_ARG => {
                expected_size = Some(value.clone());
                i += 2;
            }
            SELF_UPDATE_EXPECTED_SHA256_ARG => {
                expected_sha256 = Some(value.clone());
                i += 2;
            }
            _ => unreachable!("helper flag validated above"),
        }
    }

    let source = source.context("自更新 helper 缺少 --source 参数")?;
    let target = target.context("自更新 helper 缺少 --target 参数")?;
    // Compatibility schema default: omission means exactly the target already
    // subject to canonicalization, same-directory checks, and identity checks.
    let restart = restart.unwrap_or_else(|| target.clone());
    let expected_size = expected_size
        .context("自更新 helper 缺少 --expected-size 参数")?
        .parse::<u64>()
        .context("自更新 helper 的 --expected-size 不是有效整数")?;
    ensure!(
        expected_size > 0,
        "self-update expected size must be positive"
    );
    let expected_sha256 = validate_expected_sha256(
        &expected_sha256.context("自更新 helper 缺少 --expected-sha256 参数")?,
    )?;
    let expected = ExecutableIdentity {
        size: expected_size,
        sha256: expected_sha256,
    };

    let helper_exe = current_exe_path()?;
    let (source, target, restart) =
        validate_update_helper_paths(&source, &target, &restart, &helper_exe)?;

    apply_downloaded_update(&source, &target, &restart, &expected, channel)?;
    Ok(true)
}

/// Parse the one-shot acknowledgement challenge passed by the update helper.
///
/// A normal launch returns `None`. An update candidate must report health only
/// after its top-level window has been created; until then the helper keeps the
/// verified `.old` executable available for rollback.
pub fn startup_health_ack_from_args() -> Result<Option<StartupHealthAck>> {
    let args: Vec<String> = std::env::args().collect();
    startup_health_ack_from(&args, &current_exe_path()?)
}

fn startup_health_ack_from(args: &[String], executable: &Path) -> Result<Option<StartupHealthAck>> {
    let mut path: Option<PathBuf> = None;
    let mut token: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            SELF_UPDATE_HEALTH_ACK_ARG => {
                ensure!(path.is_none(), "自更新健康确认路径参数重复");
                let value = args
                    .get(i + 1)
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .context("自更新健康确认 --self-update-health-ack 缺少值")?;
                path = Some(PathBuf::from(value));
                i += 2;
            }
            SELF_UPDATE_HEALTH_TOKEN_ARG => {
                ensure!(token.is_none(), "自更新健康确认 token 参数重复");
                let value = args
                    .get(i + 1)
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .context("自更新健康确认 --self-update-health-token 缺少值")?;
                token = Some(value.clone());
                i += 2;
            }
            _ => i += 1,
        }
    }

    match (path, token) {
        (None, None) => Ok(None),
        (Some(path), Some(token)) => {
            let token =
                validate_expected_sha256(&token).context("自更新健康确认 token 格式无效")?;
            let path = validate_health_ack_path(&path, executable)?;
            Ok(Some(StartupHealthAck { path, token }))
        }
        _ => bail!("自更新健康确认参数不完整"),
    }
}

/// Start a lightweight watcher that acknowledges only after this process owns
/// a visible top-level window. This places the health boundary after NWG has
/// initialized and built the application UI, without making GUI code aware of
/// the self-update protocol.
pub fn acknowledge_health_when_window_ready(ack: StartupHealthAck) {
    thread::spawn(move || {
        let acknowledge = || {
            acknowledge_health_when_window_ready_with(
                &ack,
                HEALTH_CHECK_ATTEMPTS,
                HEALTH_CHECK_INTERVAL,
                process_has_visible_window,
                thread::sleep,
            )
        };
        #[cfg(windows)]
        legacy_cleanup::after_ack(
            legacy_cleanup::ParentProof::capture(),
            acknowledge,
            legacy_cleanup::ParentProof::finish,
        );
        #[cfg(not(windows))]
        acknowledge();
    });
}
fn acknowledge_health_when_window_ready_with(
    ack: &StartupHealthAck,
    attempts: usize,
    delay: Duration,
    mut visible_window: impl FnMut() -> Result<bool>,
    mut sleep: impl FnMut(Duration),
) -> bool {
    for attempt in 0..attempts {
        let visible = match visible_window() {
            Ok(visible) => visible,
            Err(error) => {
                crate::observability::event(
                    "selfupdate.health_window_failed",
                    "candidate window enumeration failed; acknowledgment withheld",
                    format!("{error:#}"),
                    "candidate window enumeration",
                    "helper rejects unacknowledged candidate",
                    ack.path.display(),
                );
                return false;
            }
        };
        if visible {
            if let Err(error) = write_health_ack(ack) {
                crate::observability::event(
                    "selfupdate.health_ack_failed",
                    "candidate window exists but acknowledgment could not be committed",
                    format!("{error:#}"),
                    ack.path.display(),
                    "helper retains backup and rejects unacknowledged candidate",
                    ack.path.display(),
                );
                return false;
            }
            return true;
        }
        if attempt + 1 < attempts {
            sleep(delay);
        }
    }
    crate::observability::event(
        "selfupdate.health_window_timeout",
        "candidate did not create a visible window within its check budget",
        format!("{attempts} window checks exhausted"),
        "candidate visible window",
        "helper rejects unacknowledged candidate",
        ack.path.display(),
    );
    false
}

fn validate_health_ack_path(path: &Path, executable: &Path) -> Result<PathBuf> {
    validate_health_ack_path_with(path, executable, || fs::symlink_metadata(path))
}

fn validate_health_ack_path_with(
    path: &Path,
    executable: &Path,
    metadata: impl FnOnce() -> std::io::Result<fs::Metadata>,
) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "自更新健康确认路径必须是绝对路径");
    match metadata() {
        Ok(_) => bail!("自更新健康确认路径已存在: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("无法检查自更新健康确认路径"),
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("无法读取自更新健康确认文件名")?;
    ensure!(
        name.starts_with(HEALTH_ACK_PREFIX) && name.ends_with(".ack"),
        "自更新健康确认文件名无效: {name}"
    );

    let executable_dir = executable
        .parent()
        .context("无法确定更新器所在目录")?
        .canonicalize()
        .context("无法解析更新器所在目录")?;
    let ack_dir = path
        .parent()
        .context("无法确定自更新健康确认目录")?
        .canonicalize()
        .context("无法解析自更新健康确认目录")?;
    ensure!(
        ack_dir == executable_dir,
        "自更新健康确认文件必须与更新器位于同一目录"
    );
    Ok(path.to_path_buf())
}

fn health_ack_contents(token: &str) -> String {
    format!("{HEALTH_ACK_HEADER}\n{token}\n")
}

fn write_health_ack(ack: &StartupHealthAck) -> Result<()> {
    let pending = ack.path.with_extension("ack.pending");
    remove_stale_file(&pending)?;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&pending)
        .with_context(|| format!("创建自更新健康确认临时文件失败: {}", pending.display()))?;
    file.write_all(health_ack_contents(&ack.token).as_bytes())
        .context("写入自更新健康确认失败")?;
    file.sync_all().context("同步自更新健康确认失败")?;
    drop(file);
    fs::rename(&pending, &ack.path).context("提交自更新健康确认失败")
}

#[cfg(windows)]
fn process_has_visible_window() -> Result<bool> {
    use winapi::shared::minwindef::{BOOL, DWORD, LPARAM};
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible};

    struct WindowSearch {
        pid: DWORD,
        found: bool,
    }

    unsafe extern "system" fn inspect_window(window: HWND, state: LPARAM) -> BOOL {
        let state = unsafe { &mut *(state as *mut WindowSearch) };
        let mut owner_pid = 0;
        unsafe {
            GetWindowThreadProcessId(window, &mut owner_pid);
            if owner_pid == state.pid && IsWindowVisible(window) != 0 {
                state.found = true;
                return 0;
            }
        }
        1
    }

    let mut state = WindowSearch {
        pid: std::process::id(),
        found: false,
    };
    let completed = unsafe {
        EnumWindows(
            Some(inspect_window),
            &mut state as *mut WindowSearch as LPARAM,
        )
    };
    window_enumeration_result(completed != 0, state.found, std::io::Error::last_os_error())
}

#[cfg(windows)]
fn window_enumeration_result(completed: bool, found: bool, error: std::io::Error) -> Result<bool> {
    if completed || found {
        Ok(found)
    } else {
        Err(error).context("EnumWindows failed")
    }
}

#[cfg(not(windows))]
fn process_has_visible_window() -> Result<bool> {
    Ok(true)
}

fn validate_update_helper_paths(
    source: &Path,
    target: &Path,
    restart: &Path,
    helper_exe: &Path,
) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let helper_exe = canonicalize_absolute(helper_exe, "自更新 helper 路径")?;
    let source = canonicalize_absolute(source, "自更新源文件")?;
    let target = canonicalize_absolute(target, "自更新目标文件")?;
    let restart = canonicalize_absolute(restart, "自更新重启目标")?;

    let helper_name = helper_exe
        .file_name()
        .and_then(|name| name.to_str())
        .context("无法读取自更新 helper 文件名")?;
    if !is_helper_file_name(helper_name) {
        bail!("拒绝非自更新 helper 执行替换: {}", helper_exe.display());
    }

    if target != restart {
        bail!(
            "自更新 target/restart 不一致: {} != {}",
            target.display(),
            restart.display()
        );
    }

    let helper_dir = helper_exe.parent().context("无法确定 helper 所在目录")?;
    let target_dir = target.parent().context("无法确定自更新目标目录")?;
    if helper_dir != target_dir {
        bail!(
            "自更新 helper 与目标不在同一目录: {} != {}",
            helper_dir.display(),
            target_dir.display()
        );
    }

    let source_dir = source.parent().context("无法确定自更新源文件目录")?;
    if source_dir != target_dir {
        bail!(
            "自更新源文件与目标不在同一目录: {} != {}",
            source_dir.display(),
            target_dir.display()
        );
    }

    let expected_source =
        canonicalize_absolute(&target.with_extension("exe.new"), "期望的自更新源文件")?;
    if source != expected_source {
        bail!(
            "自更新源文件不匹配: {} != {}",
            source.display(),
            expected_source.display()
        );
    }

    if helper_exe == target {
        bail!("自更新 helper 不能覆盖自身: {}", helper_exe.display());
    }

    Ok((source, target, restart))
}

fn canonicalize_absolute(path: &Path, label: &str) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("{label} 必须是绝对路径: {}", path.display());
    }
    fs::canonicalize(path).with_context(|| format!("{label} 不存在或无法访问: {}", path.display()))
}

fn is_helper_file_name(name: &str) -> bool {
    name == LEGACY_SELF_UPDATE_HELPER_NAME
        || (name.starts_with(SELF_UPDATE_HELPER_PREFIX) && name.ends_with(".exe"))
}

/// 清理上次自更新残留的临时文件（.new / .old / .old.pending / helper）。
///
/// 新版进程能运行到这里，说明原子替换和重启已经完成，可以删除回滚副本。
/// 如果 helper 在替换前中断，仍在运行的旧版也会清理未消费的 .new。
pub fn cleanup_old_exe() {
    cleanup_old_exe_with(current_exe_path());
}

fn cleanup_old_exe_with(exe: Result<PathBuf>) {
    let result = exe.and_then(|exe| cleanup_self_update_artifacts(&exe));
    if let Err(error) = result {
        crate::observability::event(
            "selfupdate.cleanup_failed",
            "startup artifact cleanup failed",
            format!("{error:#}"),
            "self-update artifacts",
            "retain artifacts and continue normal startup",
            "startup cleanup",
        );
    }
}

fn cleanup_self_update_artifacts(exe: &Path) -> Result<()> {
    library::ensure_process_can_update()?;
    let _transaction = acquire_update_lock(exe, 1, Duration::ZERO)?;
    library::ensure_process_can_update()?;
    let metadata = fs::symlink_metadata(exe)
        .with_context(|| format!("无法检查当前更新器文件: {}", exe.display()))?;
    ensure!(metadata.file_type().is_file(), "当前更新器不是普通文件");
    remove_inactive_helpers(exe)?;

    for path in [
        exe.with_extension("exe.new"),
        exe.with_extension("exe.old"),
        exe.with_extension("exe.old.pending"),
    ] {
        remove_stale_file(&path)?;
    }

    let parent = exe.parent().context("无法确定更新器所在目录")?;
    for entry in fs::read_dir(parent).context("无法扫描自更新 helper")? {
        let entry = entry.context("读取自更新 helper 目录项失败")?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if is_health_ack_file_name(name) {
            remove_stale_file(&path)?;
        }
    }

    Ok(())
}

/// A spawned Windows helper holds its image open even before it obtains the
/// transaction lock. Check every helper before modifying staging/backup/acks.
/// This also recognizes the legacy helper used by already deployed versions.
fn remove_inactive_helpers(exe: &Path) -> Result<()> {
    let parent = exe.parent().context("无法确定更新器所在目录")?;
    for entry in fs::read_dir(parent).context("无法扫描自更新 helper")? {
        let path = entry.context("读取自更新 helper 目录项失败")?.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_helper_file_name)
            && let Err(error) = remove_stale_file(&path)
        {
            crate::observability::event(
                "selfupdate.helper_guard_failed",
                "helper may still be running; staging and cleanup forbidden",
                format!("{error:#}"),
                path.display(),
                "retain staging, backup, and acknowledgments",
                exe.display(),
            );
            return Err(error).context("无法确认所有自更新 helper 已停止");
        }
    }
    Ok(())
}

fn remove_stale_file(path: &Path) -> Result<()> {
    remove_stale_file_with(
        path,
        || fs::symlink_metadata(path),
        || fs::remove_file(path),
    )
}

fn remove_stale_file_with(
    path: &Path,
    metadata: impl FnOnce() -> std::io::Result<fs::Metadata>,
    remove: impl FnOnce() -> std::io::Result<()>,
) -> Result<()> {
    let metadata = match metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("无法检查残留文件: {}", path.display()));
        }
    };
    ensure!(
        metadata.file_type().is_file() || metadata.file_type().is_symlink(),
        "拒绝删除非文件自更新残留项: {}",
        path.display()
    );
    match remove() {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("删除残留文件失败: {}", path.display())),
    }
}

fn is_health_ack_file_name(name: &str) -> bool {
    name.starts_with(HEALTH_ACK_PREFIX)
        && (name.ends_with(".ack") || name.ends_with(".ack.pending"))
}

/// 更新器远程版本信息（从版本信息 URL 获取）
#[derive(Debug, Deserialize)]
#[serde(try_from = "RawUpdaterVersionInfo")]
pub struct UpdaterVersionInfo {
    pub version: semver::Version,
    /// exe 下载地址（经 GitHub 下载代理）
    pub download_url: String,
    /// 构建 ID（commit SHA），所有通道统一使用
    pub build_id: String,
    /// exe 文件的 SHA256 哈希（小写十六进制），用于下载后完整性校验
    pub sha256: String,
    pub size: u64,
}

#[derive(Deserialize)]
struct RawUpdaterVersionInfo {
    version: String,
    build_id: String,
    download_url: String,
    sha256: String,
    size: u64,
}

impl TryFrom<RawUpdaterVersionInfo> for UpdaterVersionInfo {
    type Error = anyhow::Error;
    fn try_from(raw: RawUpdaterVersionInfo) -> Result<Self> {
        anyhow::ensure!(
            raw.build_id.len() == 40 && raw.build_id.bytes().all(|b| b.is_ascii_hexdigit()),
            "build_id must be a full commit SHA"
        );
        anyhow::ensure!(
            raw.sha256.len() == 64 && raw.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "sha256 must contain 64 hexadecimal digits"
        );
        anyhow::ensure!(raw.size > 0, "updater size must be positive");
        validate_release_url(&raw.download_url)?;
        Ok(Self {
            version: semver::Version::parse(&raw.version)
                .context("updater version must be SemVer")?,
            build_id: raw.build_id.to_ascii_lowercase(),
            sha256: raw.sha256.to_ascii_lowercase(),
            download_url: raw.download_url,
            size: raw.size,
        })
    }
}

fn validate_release_url(value: &str) -> Result<()> {
    let official = value
        .strip_prefix("https://gh.chenjicheng.cn/")
        .unwrap_or(value);
    let url = url::Url::parse(official).context("invalid updater Release URL")?;
    anyhow::ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "updater must use an official HTTPS Release URL"
    );
    let parts: Vec<_> = url
        .path_segments()
        .context("invalid updater Release path")?
        .collect();
    anyhow::ensure!(
        parts.len() == 6
            && parts[..4] == ["chenjicheng", "upmc", "releases", "download"]
            && !parts[4].is_empty()
            && parts[4]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
            && parts[5] == "updater.exe",
        "updater must be the official updater.exe Release artifact"
    );
    anyhow::ensure!(official == url.as_str(), "noncanonical updater Release URL");
    Ok(())
}

/// 从版本信息 URL 获取更新器版本信息（带重试）。
fn fetch_updater_info(channel: UpdateChannel) -> Result<UpdaterVersionInfo> {
    bridge_retry_with(
        config::RETRY_MAX_ATTEMPTS,
        Duration::from_secs(config::RETRY_BASE_DELAY_SECS),
        "获取更新器版本信息",
        || fetch_updater_info_inner(channel),
        thread::sleep,
    )
}

/// fetch_updater_info 的内部实现（单次尝试）。
fn fetch_updater_info_inner(channel: UpdateChannel) -> Result<UpdaterVersionInfo> {
    let url = config::updater_version_url(channel);

    let agent = bridge_http_agent(Duration::from_secs(config::HTTP_TIMEOUT_SECS));
    fetch_updater_info_from(&agent, url)
}

fn bridge_http_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(timeout))
        .build()
        .into()
}

fn fetch_updater_info_from(agent: &ureq::Agent, url: &str) -> Result<UpdaterVersionInfo> {
    let body = agent
        .get(url)
        .call()
        .context("无法连接到更新器版本服务器")?;

    let text = body
        .into_body()
        .with_config()
        .limit(16 * 1024)
        .read_to_string()
        .context("读取版本信息失败")?;

    serde_json::from_str(&text).context("解析 version.json 失败")
}

/// 检查并执行自更新。
///
/// Stable requires greater SemVer precedence; dev additionally permits an
/// explicit different build at equal precedence. Every channel rejects downgrades.
///
/// 返回 `SelfUpdateResult::Restarting` 时，调用方应立即退出进程。
pub fn check_and_update(
    channel: UpdateChannel,
    on_progress: &dyn Fn(crate::update::Progress),
) -> Result<SelfUpdateResult> {
    library::ensure_process_can_update()?;
    on_progress(crate::update::Progress::new(
        1,
        format!("检查更新器版本 ({channel})..."),
    ));

    // 从对应通道的 version.json 获取版本信息
    let info = fetch_updater_info(channel)?;

    // Validate metadata before selecting a forward-only update.
    let needs_update =
        bridge_update_required(&info, env!("CARGO_PKG_VERSION"), CURRENT_BUILD_ID, channel)?;

    if !needs_update {
        return Ok(SelfUpdateResult::UpToDate);
    }

    let local_id = CURRENT_BUILD_ID.unwrap_or("local");
    let remote_id = &info.build_id;
    on_progress(crate::update::Progress::new(
        2,
        format!("发现新版本 {local_id} → {remote_id}，正在下载..."),
    ));

    let exe_path = current_exe_path()?;
    let _transaction = acquire_update_lock(&exe_path, 1, Duration::ZERO)?;
    library::ensure_process_can_update()?;
    remove_inactive_helpers(&exe_path)?;
    on_progress(crate::update::Progress::new(
        5,
        "正在下载、校验并安装更新器...",
    ));
    library::apply(&info, &exe_path, channel)?;
    on_progress(crate::update::Progress::new(
        11,
        "新版已启动并通过健康确认，正在退出旧版...",
    ));
    Ok(SelfUpdateResult::Restarting)
}

fn bridge_update_required(
    info: &UpdaterVersionInfo,
    current: &str,
    local: Option<&str>,
    channel: UpdateChannel,
) -> Result<bool> {
    let current =
        semver::Version::parse(current).context("current updater version must be SemVer")?;
    let ordering = info.version.cmp_precedence(&current);
    Ok(ordering.is_gt()
        || (ordering.is_eq()
            && channel == UpdateChannel::Dev
            && local.is_some_and(|local| !info.build_id.eq_ignore_ascii_case(local))))
}

#[cfg(test)]
#[path = "selfupdate_bridge_tests.rs"]
mod bridge_tests;

#[cfg(test)]
#[path = "selfupdate_helper_regression_tests.rs"]
mod helper_regression_tests;

fn restart_command(path: &Path, channel: Option<UpdateChannel>) -> Command {
    let mut command = Command::new(path);
    if let Some(channel) = channel {
        command.arg("--channel").arg(channel.to_string());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(config::CREATE_NO_WINDOW);
    }
    command
}

fn prepare_update_helper(helper_path: &Path, copy: impl FnOnce() -> Result<()>) -> Result<()> {
    let result =
        copy().with_context(|| format!("创建自更新 helper 失败: {}", helper_path.display()));
    if result.is_err() {
        crate::observability::cleanup(remove_stale_file(helper_path), helper_path.display());
    }
    result
}

fn unique_helper_file_name() -> String {
    format!(
        "{SELF_UPDATE_HELPER_PREFIX}{}.exe",
        crate::observability::unique_id()
    )
}

fn copy_file_with_retry(
    source: &Path,
    dest: &Path,
    attempts: usize,
    delay: Duration,
) -> Result<()> {
    copy_file_with_retry_with(
        source,
        dest,
        attempts,
        delay,
        || fs::copy(source, dest),
        thread::sleep,
    )
}

fn copy_file_with_retry_with(
    source: &Path,
    dest: &Path,
    attempts: usize,
    delay: Duration,
    mut copy: impl FnMut() -> std::io::Result<u64>,
    mut sleep: impl FnMut(Duration),
) -> Result<()> {
    retry_filesystem_operation(
        "selfupdate.copy.retry",
        "selfupdate.copy.failed",
        source,
        dest,
        attempts,
        delay,
        || copy().map(|_| ()).map_err(Into::into),
        |error| Ok(is_transient_file_error(error, false)),
        &mut sleep,
    )
}

/// Retry only bounded sharing/locking interruptions. Windows reports an image
/// section held by the departing updater as ACCESS_DENIED as well as a sharing
/// violation, so replacement/restoration alone retain that bounded exception.
fn is_transient_file_error(error: &anyhow::Error, replacing_image: bool) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
        ) || matches!(error.raw_os_error(), Some(32 | 33))
            || (replacing_image && error.raw_os_error() == Some(5))
    })
}

#[allow(clippy::too_many_arguments)]
fn retry_filesystem_operation(
    retry_id: &'static str,
    failure_id: &'static str,
    source: &Path,
    target: &Path,
    attempts: usize,
    delay: Duration,
    mut operation: impl FnMut() -> Result<()>,
    mut retry_allowed: impl FnMut(&anyhow::Error) -> Result<bool>,
    mut sleep: impl FnMut(Duration),
) -> Result<()> {
    ensure!(attempts > 0, "filesystem attempt budget must be positive");
    let mut errors = Vec::new();
    for attempt in 1..=attempts {
        match operation() {
            Ok(()) => {
                if !errors.is_empty() {
                    crate::observability::event(
                        "selfupdate.filesystem_recovered",
                        "bounded filesystem retry succeeded",
                        errors.join("; "),
                        source.display(),
                        target.display(),
                        format!("{retry_id}; attempt {attempt}"),
                    );
                }
                return Ok(());
            }
            Err(mut error) => {
                let can_retry = match retry_allowed(&error) {
                    Ok(value) => value,
                    Err(status_error) => {
                        error =
                            error.context(format!("retry safety check failed: {status_error:#}"));
                        false
                    }
                };
                errors.push(format!("attempt {attempt}: {error:#}"));
                let retrying = can_retry && attempt < attempts;
                crate::observability::event(
                    if retrying { retry_id } else { failure_id },
                    if retrying {
                        "transient filesystem error; bounded retry"
                    } else {
                        "retry unsafe, permanent failure, or attempt budget exhausted"
                    },
                    errors.join("; "),
                    source.display(),
                    if retrying {
                        format!(
                            "{}; attempt {} of {attempts}; delay {}ms",
                            target.display(),
                            attempt + 1,
                            delay.as_millis()
                        )
                    } else {
                        format!("{}; return failure", target.display())
                    },
                    target.display(),
                );
                if !retrying {
                    return Err(error.context(errors.join("; ")));
                }
                sleep(delay);
            }
        }
    }
    unreachable!("positive attempt budget returns on success or its last failure")
}

/// helper 进程执行的替换逻辑。
///
/// Windows 会锁定正在运行的 exe，因此这里不依赖 PID，也不调用 PowerShell；
/// `ReplaceFileW` 在同一卷内一次提交新版和 `.old` 回滚副本，避免覆盖中断留下
/// 半个可执行文件。helper 保留回滚副本并监督新版，只有新版创建可见窗口并
/// 返回本次更新专属的健康确认后才结束监督。
fn apply_downloaded_update(
    source: &Path,
    target: &Path,
    restart: &Path,
    expected: &ExecutableIdentity,
    channel: Option<UpdateChannel>,
) -> Result<()> {
    let _transaction = acquire_update_lock(target, REPLACEMENT_ATTEMPTS, REPLACEMENT_RETRY_DELAY)?;
    let ack = new_startup_health_ack(target, expected)?;
    let result = apply_downloaded_update_with(
        source,
        target,
        restart,
        expected,
        &ack,
        UpdateHooks {
            replace: |source: &Path, target: &Path, backup: &Path| {
                replace_file_with_backup_with_retry(
                    source,
                    target,
                    backup,
                    REPLACEMENT_ATTEMPTS,
                    REPLACEMENT_RETRY_DELAY,
                )
            },
            launch_candidate: |path: &Path, ack: &StartupHealthAck| {
                restart_command(path, channel)
                    .arg(SELF_UPDATE_HEALTH_ACK_ARG)
                    .arg(&ack.path)
                    .arg(SELF_UPDATE_HEALTH_TOKEN_ARG)
                    .arg(&ack.token)
                    .spawn()
            },
            supervise: |child: &mut Child, ack: &StartupHealthAck| {
                wait_for_candidate_health(child, ack)
            },
            stop_candidate: |child: &mut Child| terminate_candidate(child),
            launch_previous: |path: &Path| restart_command(path, channel).spawn().map(|_| ()),
        },
    );
    finish_health_supervision(&ack, result)
}

fn finish_health_supervision(ack: &StartupHealthAck, result: Result<()>) -> Result<()> {
    crate::observability::cleanup(remove_stale_file(&ack.path), ack.path.display());
    let pending = ack.path.with_extension("ack.pending");
    crate::observability::cleanup(remove_stale_file(&pending), pending.display());
    result
}

/// The persistent file is never unlinked: every process must lock the same inode.
/// File ownership releases the OS lock on every return/unwind/process exit.
fn acquire_update_lock(target: &Path, attempts: usize, delay: Duration) -> Result<fs::File> {
    let path = target.with_extension("exe.update.lock");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_file(),
            "update lock must be a regular file: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect update transaction lock"),
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .context("cannot open update transaction lock")?;
    retry_filesystem_operation(
        "selfupdate.lock.retry",
        "selfupdate.lock.failed",
        target,
        &path,
        attempts,
        delay,
        || fs2::FileExt::try_lock_exclusive(&file).map_err(Into::into),
        |error| Ok(is_transient_file_error(error, false)),
        thread::sleep,
    )?;
    Ok(file)
}

struct UpdateHooks<Replace, LaunchCandidate, Supervise, Stop, LaunchPrevious> {
    replace: Replace,
    launch_candidate: LaunchCandidate,
    supervise: Supervise,
    stop_candidate: Stop,
    launch_previous: LaunchPrevious,
}

fn apply_downloaded_update_with<
    Replace,
    LaunchCandidate,
    Supervise,
    Stop,
    LaunchPrevious,
    Process,
>(
    source: &Path,
    target: &Path,
    restart: &Path,
    expected: &ExecutableIdentity,
    ack: &StartupHealthAck,
    hooks: UpdateHooks<Replace, LaunchCandidate, Supervise, Stop, LaunchPrevious>,
) -> Result<()>
where
    Replace: FnMut(&Path, &Path, &Path) -> Result<()>,
    LaunchCandidate: FnMut(&Path, &StartupHealthAck) -> std::io::Result<Process>,
    Supervise: FnMut(&mut Process, &StartupHealthAck) -> Result<()>,
    Stop: FnMut(&mut Process) -> Result<()>,
    LaunchPrevious: FnMut(&Path) -> std::io::Result<()>,
{
    let UpdateHooks {
        mut replace,
        mut launch_candidate,
        mut supervise,
        mut stop_candidate,
        mut launch_previous,
    } = hooks;
    let previous = executable_identity(target).context("读取旧版更新器身份失败")?;
    let backup = target.with_extension("exe.old");
    remove_stale_file(&backup).context("清理旧的自更新回滚副本失败")?;
    remove_stale_file(&target.with_extension("exe.old.pending"))
        .context("清理旧的自更新回滚临时文件失败")?;
    if let Err(error) = verify_executable_identity(source, expected) {
        return fail_update_and_restart_previous(
            format!("替换前校验更新器临时文件失败: {error:#}"),
            target,
            &backup,
            &previous,
            restart,
            &mut launch_previous,
        );
    }

    if let Err(error) = replace(source, target, &backup) {
        return fail_update_and_restart_previous(
            format!("自更新原子替换失败: {error:#}"),
            target,
            &backup,
            &previous,
            restart,
            &mut launch_previous,
        );
    }

    if let Err(error) = verify_executable_identity(&backup, &previous) {
        return fail_update_and_restart_previous(
            format!("自更新回滚副本校验失败: {error:#}"),
            target,
            &backup,
            &previous,
            restart,
            &mut launch_previous,
        );
    }

    if let Err(error) = verify_executable_identity(target, expected) {
        return fail_update_and_restart_previous(
            format!("已安装更新器校验失败: {error:#}"),
            target,
            &backup,
            &previous,
            restart,
            &mut launch_previous,
        );
    }

    let mut candidate = match launch_candidate(restart, ack) {
        Ok(candidate) => candidate,
        Err(error) => {
            return fail_update_and_restart_previous(
                format!("启动新版更新器失败: {}: {error}", restart.display()),
                target,
                &backup,
                &previous,
                restart,
                &mut launch_previous,
            );
        }
    };

    if let Err(error) = supervise(&mut candidate, ack) {
        let message = format!("新版更新器健康确认失败: {error:#}");
        if let Err(stop_error) = stop_candidate(&mut candidate) {
            crate::observability::event(
                "selfupdate.candidate_stop_failed",
                "candidate may still run; restoration and relaunch forbidden",
                format!("{error:#}; termination: {stop_error:#}"),
                target.display(),
                "retain candidate and verified backup for recovery",
                target.display(),
            );
            bail!(
                "{message}\n终止新版更新器失败: {stop_error:#}\n已保留新版和回滚副本；确认新版停止前禁止恢复或启动第二个更新器"
            );
        }
        return fail_update_and_restart_previous(
            message,
            target,
            &backup,
            &previous,
            restart,
            &mut launch_previous,
        );
    }

    Ok(())
}

fn new_startup_health_ack(
    target: &Path,
    expected: &ExecutableIdentity,
) -> Result<StartupHealthAck> {
    new_startup_health_ack_at(target, expected, SystemTime::now())
}

fn new_startup_health_ack_at(
    target: &Path,
    expected: &ExecutableIdentity,
    now: SystemTime,
) -> Result<StartupHealthAck> {
    let parent = target.parent().context("无法确定更新器所在目录")?;
    static ACK_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let timestamp = match now.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos().to_string(),
        Err(error) => format!("before-{}", error.duration().as_nanos()),
    };
    let nonce = format!(
        "{timestamp}-{}",
        ACK_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let mut hasher = Sha256::new();
    hasher.update(expected.sha256.as_bytes());
    hasher.update(expected.size.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(nonce.as_bytes());
    hasher.update(target.as_os_str().to_string_lossy().as_bytes());
    let token = format!("{:x}", hasher.finalize());
    let path = parent.join(format!(
        "{HEALTH_ACK_PREFIX}{}-{nonce}.ack",
        std::process::id()
    ));
    remove_stale_file(&path)?;
    remove_stale_file(&path.with_extension("ack.pending"))?;
    Ok(StartupHealthAck { path, token })
}

fn read_health_ack(ack: &StartupHealthAck) -> Result<bool> {
    match fs::read_to_string(&ack.path) {
        Ok(contents) => {
            ensure!(
                contents == health_ack_contents(&ack.token),
                "自更新健康确认内容无效"
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("读取自更新健康确认失败: {}", ack.path.display()))
        }
    }
}

fn wait_for_candidate_health(child: &mut Child, ack: &StartupHealthAck) -> Result<()> {
    wait_for_candidate_health_with(
        ack,
        HEALTH_CHECK_ATTEMPTS,
        HEALTH_CHECK_INTERVAL,
        || child.try_wait().map(|status| status.is_some()),
        read_health_ack,
        thread::sleep,
    )
}

fn wait_for_candidate_health_with<Exited, CheckAck, Sleep>(
    ack: &StartupHealthAck,
    attempts: usize,
    delay: Duration,
    mut has_exited: Exited,
    mut check_ack: CheckAck,
    mut sleep: Sleep,
) -> Result<()>
where
    Exited: FnMut() -> std::io::Result<bool>,
    CheckAck: FnMut(&StartupHealthAck) -> Result<bool>,
    Sleep: FnMut(Duration),
{
    ensure!(attempts > 0, "health check attempt budget must be positive");
    for attempt in 0..attempts {
        if has_exited().context("检查新版更新器进程状态失败")? {
            bail!("新版更新器在健康确认前提前退出");
        }
        if check_ack(ack)? {
            ensure!(
                !has_exited().context("复核新版更新器进程状态失败")?,
                "新版更新器发送健康确认后立即退出"
            );
            return Ok(());
        }
        if attempt + 1 < attempts {
            sleep(delay);
        }
    }
    bail!(
        "等待新版更新器健康确认超时（{} 毫秒）",
        delay.as_millis() * attempts.saturating_sub(1) as u128
    )
}

fn terminate_candidate(child: &mut Child) -> Result<()> {
    if child
        .try_wait()
        .context("检查新版更新器退出状态失败")?
        .is_none()
    {
        child.kill().context("终止新版更新器失败")?;
    }
    child.wait().context("等待新版更新器退出失败")?;
    Ok(())
}

fn fail_update_and_restart_previous<Launch>(
    message: String,
    target: &Path,
    backup: &Path,
    previous: &ExecutableIdentity,
    restart: &Path,
    launch: &mut Launch,
) -> Result<()>
where
    Launch: FnMut(&Path) -> std::io::Result<()>,
{
    crate::observability::event(
        "selfupdate.rollback",
        "candidate failed; restore and restart verified previous updater",
        &message,
        target.display(),
        backup.display(),
        restart.display(),
    );
    if let Err(recovery_error) = restore_previous_version(target, backup, previous) {
        crate::observability::event(
            "selfupdate.rollback_failed",
            "verified previous updater could not be restored",
            format!("{message}; {recovery_error:#}"),
            target.display(),
            backup.display(),
            restart.display(),
        );
        bail!("{message}\n恢复旧版更新器失败: {recovery_error:#}");
    }

    match launch(restart) {
        Ok(()) => bail!(
            "{message}\n已恢复并重新启动旧版更新器: {}",
            restart.display()
        ),
        Err(restart_error) => {
            crate::observability::event(
                "selfupdate.restart_failed",
                "previous updater restored but restart failed",
                format!("{message}; {restart_error}"),
                target.display(),
                restart.display(),
                restart.display(),
            );
            bail!(
                "{message}\n旧版已恢复，但重新启动失败: {}: {restart_error}",
                restart.display()
            );
        }
    }
}

fn restore_previous_version(
    target: &Path,
    backup: &Path,
    previous: &ExecutableIdentity,
) -> Result<()> {
    let target_error = match verify_executable_identity(target, previous) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    crate::observability::event(
        "selfupdate.restore_required",
        "target is not the verified previous updater; validate backup before restoration",
        format!("{target_error:#}"),
        target.display(),
        backup.display(),
        target.display(),
    );
    let recovery = (|| {
        verify_executable_identity(backup, previous).context("回滚副本不可用")?;
        atomic_restore_backup_with_retry(backup, target, 10, Duration::from_millis(300))
            .context("原子恢复回滚副本失败")?;
        verify_executable_identity(target, previous).context("恢复后的旧版更新器校验失败")
    })();
    recovery.map_err(|error: anyhow::Error| {
        error.context(format!(
            "previous target validation failed: {target_error:#}"
        ))
    })
}

fn replace_file_with_backup_with_retry(
    source: &Path,
    target: &Path,
    backup: &Path,
    attempts: usize,
    delay: Duration,
) -> Result<()> {
    replace_file_with_backup_with_retry_with(
        source,
        target,
        backup,
        attempts,
        delay,
        || replace_file_with_backup(source, target, backup),
        || fs::symlink_metadata(backup),
        thread::sleep,
    )
}

// Keep the Windows replacement, metadata and wait seams explicit so tests can
// independently inject primary and secondary failures without real replacement.
#[allow(clippy::too_many_arguments)]
fn replace_file_with_backup_with_retry_with(
    source: &Path,
    target: &Path,
    backup: &Path,
    attempts: usize,
    delay: Duration,
    mut replace: impl FnMut() -> Result<()>,
    mut backup_metadata: impl FnMut() -> std::io::Result<fs::Metadata>,
    mut sleep: impl FnMut(Duration),
) -> Result<()> {
    retry_filesystem_operation(
        "selfupdate.replace.retry",
        "selfupdate.replace.failed",
        source,
        target,
        attempts,
        delay,
        &mut replace,
        |error| {
            match backup_metadata() {
                Ok(_) => Ok(false), // A backup means ReplaceFile may have partially committed.
                Err(status_error) if status_error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(is_transient_file_error(error, true))
                }
                Err(status_error) => Err(status_error).with_context(|| {
                    format!("cannot establish backup state: {}", backup.display())
                }),
            }
        },
        &mut sleep,
    )
}

#[cfg(windows)]
fn replace_file_with_backup(source: &Path, target: &Path, backup: &Path) -> Result<()> {
    use std::ffi::c_void;
    use std::ptr;
    use winapi::um::winbase::REPLACEFILE_WRITE_THROUGH;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "ReplaceFileW"]
        fn replace_file_w(
            replaced: *const u16,
            replacement: *const u16,
            backup: *const u16,
            flags: u32,
            exclude: *mut c_void,
            reserved: *mut c_void,
        ) -> i32;
    }

    let target = wide_null(target);
    let source = wide_null(source);
    let backup = wide_null(backup);
    let success = unsafe {
        replace_file_w(
            target.as_ptr(),
            source.as_ptr(),
            backup.as_ptr(),
            REPLACEFILE_WRITE_THROUGH,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if success == 0 {
        return Err(std::io::Error::last_os_error()).context("ReplaceFileW 失败");
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file_with_backup(source: &Path, target: &Path, backup: &Path) -> Result<()> {
    let pending = target.with_extension("exe.old.pending");
    remove_stale_file(&pending)?;
    fs::copy(target, &pending).context("创建回滚临时副本失败")?;
    fs::OpenOptions::new()
        .write(true)
        .open(&pending)
        .context("打开回滚临时副本失败")?
        .sync_all()
        .context("同步回滚临时副本失败")?;
    fs::rename(&pending, backup).context("提交回滚副本失败")?;
    fs::rename(source, target).context("原子替换更新器失败")
}

fn atomic_restore_backup_with_retry(
    backup: &Path,
    target: &Path,
    attempts: usize,
    delay: Duration,
) -> Result<()> {
    atomic_restore_backup_with_retry_with(
        backup,
        target,
        attempts,
        delay,
        || atomic_restore_backup(backup, target),
        thread::sleep,
    )
}

fn atomic_restore_backup_with_retry_with(
    backup: &Path,
    target: &Path,
    attempts: usize,
    delay: Duration,
    mut restore: impl FnMut() -> Result<()>,
    mut sleep: impl FnMut(Duration),
) -> Result<()> {
    retry_filesystem_operation(
        "selfupdate.restore.retry",
        "selfupdate.restore.failed",
        backup,
        target,
        attempts,
        delay,
        &mut restore,
        |error| Ok(is_transient_file_error(error, true)),
        &mut sleep,
    )
}

#[cfg(windows)]
fn atomic_restore_backup(backup: &Path, target: &Path) -> Result<()> {
    use winapi::um::winbase::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let backup = wide_null(backup);
    let target = wide_null(target);
    let success = unsafe {
        MoveFileExW(
            backup.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if success == 0 {
        return Err(std::io::Error::last_os_error()).context("MoveFileExW 回滚失败");
    }
    Ok(())
}

#[cfg(not(windows))]
fn atomic_restore_backup(backup: &Path, target: &Path) -> Result<()> {
    fs::rename(backup, target).context("原子恢复旧版更新器失败")
}

#[cfg(windows)]
fn wide_null(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn validate_expected_sha256(expected: &str) -> Result<String> {
    ensure!(
        expected.len() == 64
            && expected
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "自更新 helper 的 SHA256 必须是 64 位小写十六进制"
    );
    Ok(expected.to_owned())
}

fn executable_identity(path: &Path) -> Result<ExecutableIdentity> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("读取可执行文件信息失败: {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "可执行文件不是普通文件: {}",
        path.display()
    );

    let mut file =
        fs::File::open(path).with_context(|| format!("读取可执行文件失败: {}", path.display()))?;
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic)
        .with_context(|| format!("读取 PE 文件头失败: {}", path.display()))?;
    ensure!(
        &magic == b"MZ",
        "文件不是有效的 PE 可执行文件: {}",
        path.display()
    );

    Ok(ExecutableIdentity {
        size: metadata.len(),
        sha256: calculate_file_sha256(path)?,
    })
}

fn verify_executable_identity(path: &Path, expected: &ExecutableIdentity) -> Result<()> {
    let actual = executable_identity(path)?;
    ensure!(
        actual.size == expected.size,
        "可执行文件大小校验失败: {}，期望 {}，实际 {}",
        path.display(),
        expected.size,
        actual.size
    );
    ensure!(
        actual.sha256 == expected.sha256,
        "SHA256 校验失败: {}，期望 {}，实际 {}",
        path.display(),
        expected.sha256,
        actual.sha256
    );
    Ok(())
}

fn calculate_file_sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("读取下载文件用于校验失败: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];

    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("读取下载文件用于校验失败: {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_dir(name: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "upmc_selfupdate_{name}_{}_{}",
            std::process::id(),
            millis
        ))
    }

    #[test]
    fn validate_helper_paths_accepts_expected_layout() {
        let dir = unique_test_dir("valid");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("upmc.exe");
        let source = dir.join("upmc.exe.new");
        let helper = dir.join("upmc-update-helper-1-2.exe");
        fs::write(&target, b"old").unwrap();
        fs::write(&source, b"new").unwrap();
        fs::write(&helper, b"helper").unwrap();

        let result = validate_update_helper_paths(&source, &target, &target, &helper);
        assert!(result.is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_helper_paths_rejects_unexpected_source() {
        let dir = unique_test_dir("bad_source");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("upmc.exe");
        let source = dir.join("other.exe.new");
        let helper = dir.join("upmc-update-helper-1-2.exe");
        fs::write(&target, b"old").unwrap();
        fs::write(&source, b"new").unwrap();
        fs::write(&helper, b"helper").unwrap();

        let result = validate_update_helper_paths(&source, &target, &target, &helper);
        assert!(result.is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_helper_paths_rejects_different_restart() {
        let dir = unique_test_dir("bad_restart");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("upmc.exe");
        let restart = dir.join("other.exe");
        let source = dir.join("upmc.exe.new");
        let helper = dir.join("upmc-update-helper-1-2.exe");
        fs::write(&target, b"old").unwrap();
        fs::write(&restart, b"other").unwrap();
        fs::write(&source, b"new").unwrap();
        fs::write(&helper, b"helper").unwrap();

        let result = validate_update_helper_paths(&source, &target, &restart, &helper);
        assert!(result.is_err());
        fs::remove_dir_all(&dir).ok();
    }

    fn identity(bytes: &[u8]) -> ExecutableIdentity {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        ExecutableIdentity {
            size: bytes.len() as u64,
            sha256: format!("{:x}", hasher.finalize()),
        }
    }

    #[test]
    fn atomic_update_keeps_verified_backup_until_next_startup() {
        let dir = unique_test_dir("atomic_success");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("upmc.exe");
        let source = dir.join("upmc.exe.new");
        let backup = dir.join("upmc.exe.old");
        let old_bytes = b"MZold updater";
        let new_bytes = b"MZnew authenticated updater";
        fs::write(&target, old_bytes).unwrap();
        fs::write(&source, new_bytes).unwrap();
        let mut launches = 0;
        let ack = new_startup_health_ack(&target, &identity(new_bytes)).unwrap();

        apply_downloaded_update_with(
            &source,
            &target,
            &target,
            &identity(new_bytes),
            &ack,
            UpdateHooks {
                replace: |source: &Path, target: &Path, backup: &Path| {
                    replace_file_with_backup_with_retry(source, target, backup, 1, Duration::ZERO)
                },
                launch_candidate: |path: &Path, received_ack: &StartupHealthAck| {
                    launches += 1;
                    assert_eq!(path, target);
                    assert_eq!(received_ack, &ack);
                    assert_eq!(fs::read(path).unwrap(), new_bytes);
                    Ok(())
                },
                supervise: |_process: &mut (), received_ack: &StartupHealthAck| {
                    assert_eq!(received_ack, &ack);
                    Ok(())
                },
                stop_candidate: |_process: &mut ()| {
                    panic!("healthy candidate must not be terminated")
                },
                launch_previous: |_path: &Path| {
                    panic!("healthy update must not restart the previous executable")
                },
            },
        )
        .unwrap();

        assert_eq!(launches, 1);
        assert_eq!(fs::read(&target).unwrap(), new_bytes);
        assert_eq!(fs::read(&backup).unwrap(), old_bytes);
        assert!(!source.exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn candidate_launch_failure_atomically_restores_and_restarts_old_version() {
        let dir = unique_test_dir("restart_rollback");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("upmc.exe");
        let source = dir.join("upmc.exe.new");
        let backup = dir.join("upmc.exe.old");
        let old_bytes = b"MZold updater";
        let new_bytes = b"MZnew authenticated updater";
        fs::write(&target, old_bytes).unwrap();
        fs::write(&source, new_bytes).unwrap();
        let launched_contents = std::cell::RefCell::new(Vec::new());
        let ack = new_startup_health_ack(&target, &identity(new_bytes)).unwrap();

        let result = apply_downloaded_update_with(
            &source,
            &target,
            &target,
            &identity(new_bytes),
            &ack,
            UpdateHooks {
                replace: |source: &Path, target: &Path, backup: &Path| {
                    replace_file_with_backup_with_retry(source, target, backup, 1, Duration::ZERO)
                },
                launch_candidate: |path: &Path, _ack: &StartupHealthAck| {
                    launched_contents.borrow_mut().push(fs::read(path).unwrap());
                    Err::<(), _>(std::io::Error::other("simulated launch failure"))
                },
                supervise: |_process: &mut (), _ack: &StartupHealthAck| Ok(()),
                stop_candidate: |_process: &mut ()| Ok(()),
                launch_previous: |path: &Path| {
                    launched_contents.borrow_mut().push(fs::read(path).unwrap());
                    Ok(())
                },
            },
        );

        assert!(result.is_err());
        assert_eq!(
            *launched_contents.borrow(),
            vec![new_bytes.to_vec(), old_bytes.to_vec()]
        );
        assert_eq!(fs::read(&target).unwrap(), old_bytes);
        assert!(!backup.exists());
        assert!(!source.exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn damaged_staging_file_never_replaces_old_version() {
        let dir = unique_test_dir("staging_failure");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("upmc.exe");
        let source = dir.join("upmc.exe.new");
        let old_bytes = b"MZold updater";
        let expected_new = b"MZexpected authenticated updater";
        fs::write(&target, old_bytes).unwrap();
        fs::write(&source, b"MZdamaged staging file").unwrap();
        let mut launched_contents = Vec::new();
        let ack = new_startup_health_ack(&target, &identity(expected_new)).unwrap();

        let result = apply_downloaded_update_with(
            &source,
            &target,
            &target,
            &identity(expected_new),
            &ack,
            UpdateHooks {
                replace: |_source: &Path, _target: &Path, _backup: &Path| {
                    panic!("replacement must not be attempted")
                },
                launch_candidate: |_path: &Path, _ack: &StartupHealthAck| -> std::io::Result<()> {
                    panic!("candidate must not launch with damaged staging")
                },
                supervise: |_process: &mut (), _ack: &StartupHealthAck| Ok(()),
                stop_candidate: |_process: &mut ()| Ok(()),
                launch_previous: |path: &Path| {
                    launched_contents.push(fs::read(path).unwrap());
                    Ok(())
                },
            },
        );

        assert!(result.is_err());
        assert_eq!(launched_contents, vec![old_bytes.to_vec()]);
        assert_eq!(fs::read(&target).unwrap(), old_bytes);
        assert_eq!(fs::read(&source).unwrap(), b"MZdamaged staging file");
        assert!(!target.with_extension("exe.old").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn installed_digest_failure_restores_old_version_before_restart() {
        let dir = unique_test_dir("verification_rollback");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("upmc.exe");
        let source = dir.join("upmc.exe.new");
        let old_bytes = b"MZold updater";
        let new_bytes = b"MZnew authenticated updater";
        fs::write(&target, old_bytes).unwrap();
        fs::write(&source, new_bytes).unwrap();
        let mut launched_contents = Vec::new();
        let ack = new_startup_health_ack(&target, &identity(new_bytes)).unwrap();

        let result = apply_downloaded_update_with(
            &source,
            &target,
            &target,
            &identity(new_bytes),
            &ack,
            UpdateHooks {
                replace: |source: &Path, target: &Path, backup: &Path| {
                    replace_file_with_backup_with_retry(source, target, backup, 1, Duration::ZERO)?;
                    fs::write(target, b"MZbad authenticated updater")
                        .context("simulate post-replacement corruption")?;
                    Ok(())
                },
                launch_candidate: |_path: &Path, _ack: &StartupHealthAck| -> std::io::Result<()> {
                    panic!("candidate must not launch after installed digest failure")
                },
                supervise: |_process: &mut (), _ack: &StartupHealthAck| Ok(()),
                stop_candidate: |_process: &mut ()| Ok(()),
                launch_previous: |path: &Path| {
                    launched_contents.push(fs::read(path).unwrap());
                    Ok(())
                },
            },
        );

        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains("SHA256"));
        assert_eq!(launched_contents, vec![old_bytes.to_vec()]);
        assert_eq!(fs::read(&target).unwrap(), old_bytes);
        assert!(!target.with_extension("exe.old").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn health_supervision_accepts_acknowledgement_from_running_candidate() {
        let dir = unique_test_dir("health_success");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("upmc.exe");
        fs::write(&target, b"MZcandidate").unwrap();
        let ack = new_startup_health_ack(&target, &identity(b"MZcandidate")).unwrap();
        let checks = std::cell::Cell::new(0);
        let sleeps = std::cell::Cell::new(0);

        wait_for_candidate_health_with(
            &ack,
            3,
            Duration::from_millis(1),
            || Ok(false),
            |_ack| {
                let next = checks.get() + 1;
                checks.set(next);
                Ok(next == 2)
            },
            |_delay| sleeps.set(sleeps.get() + 1),
        )
        .unwrap();

        assert_eq!(checks.get(), 2);
        assert_eq!(sleeps.get(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn health_acknowledgement_is_committed_atomically_and_verified() {
        let dir = unique_test_dir("health_ack_file");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("upmc.exe");
        fs::write(&target, b"MZcandidate").unwrap();
        let ack = new_startup_health_ack(&target, &identity(b"MZcandidate")).unwrap();

        write_health_ack(&ack).unwrap();

        assert!(read_health_ack(&ack).unwrap());
        assert_eq!(
            fs::read_to_string(&ack.path).unwrap(),
            health_ack_contents(&ack.token)
        );
        assert!(!ack.path.with_extension("ack.pending").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn health_supervision_rejects_early_candidate_exit() {
        let ack = StartupHealthAck {
            path: PathBuf::from("unused.ack"),
            token: "0".repeat(64),
        };
        let error = wait_for_candidate_health_with(
            &ack,
            3,
            Duration::ZERO,
            || Ok(true),
            |_ack| panic!("ack must not be accepted from an exited process"),
            |_delay| {},
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("提前退出"));
    }

    #[test]
    fn early_candidate_exit_recovers_and_restarts_previous_version() {
        let dir = unique_test_dir("health_early_exit");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("upmc.exe");
        let source = dir.join("upmc.exe.new");
        let old_bytes = b"MZold updater";
        let new_bytes = b"MZnew authenticated updater";
        fs::write(&target, old_bytes).unwrap();
        fs::write(&source, new_bytes).unwrap();
        let ack = new_startup_health_ack(&target, &identity(new_bytes)).unwrap();
        let stopped = std::cell::Cell::new(false);
        let restarted_old = std::cell::Cell::new(false);

        let result = apply_downloaded_update_with(
            &source,
            &target,
            &target,
            &identity(new_bytes),
            &ack,
            UpdateHooks {
                replace: |source: &Path, target: &Path, backup: &Path| {
                    replace_file_with_backup_with_retry(source, target, backup, 1, Duration::ZERO)
                },
                launch_candidate: |_path: &Path, _ack: &StartupHealthAck| Ok(()),
                supervise: |_process: &mut (), ack: &StartupHealthAck| {
                    wait_for_candidate_health_with(
                        ack,
                        1,
                        Duration::ZERO,
                        || Ok(true),
                        |_ack| Ok(false),
                        |_delay| {},
                    )
                },
                stop_candidate: |_process: &mut ()| {
                    stopped.set(true);
                    Ok(())
                },
                launch_previous: |path: &Path| {
                    restarted_old.set(true);
                    assert_eq!(fs::read(path).unwrap(), old_bytes);
                    Ok(())
                },
            },
        );

        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains("提前退出"));
        assert!(stopped.get());
        assert!(restarted_old.get());
        assert_eq!(fs::read(&target).unwrap(), old_bytes);
        assert!(!target.with_extension("exe.old").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn health_timeout_terminates_candidate_and_recovers_previous_version() {
        let dir = unique_test_dir("health_timeout");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("upmc.exe");
        let source = dir.join("upmc.exe.new");
        let old_bytes = b"MZold updater";
        let new_bytes = b"MZnew authenticated updater";
        fs::write(&target, old_bytes).unwrap();
        fs::write(&source, new_bytes).unwrap();
        let ack = new_startup_health_ack(&target, &identity(new_bytes)).unwrap();
        let stopped = std::cell::Cell::new(false);
        let restarted_old = std::cell::Cell::new(false);

        let result = apply_downloaded_update_with(
            &source,
            &target,
            &target,
            &identity(new_bytes),
            &ack,
            UpdateHooks {
                replace: |source: &Path, target: &Path, backup: &Path| {
                    replace_file_with_backup_with_retry(source, target, backup, 1, Duration::ZERO)
                },
                launch_candidate: |_path: &Path, _ack: &StartupHealthAck| Ok(()),
                supervise: |_process: &mut (), ack: &StartupHealthAck| {
                    wait_for_candidate_health_with(
                        ack,
                        2,
                        Duration::from_millis(1),
                        || Ok(false),
                        |_ack| Ok(false),
                        |_delay| {},
                    )
                },
                stop_candidate: |_process: &mut ()| {
                    stopped.set(true);
                    Ok(())
                },
                launch_previous: |path: &Path| {
                    restarted_old.set(true);
                    assert_eq!(fs::read(path).unwrap(), old_bytes);
                    Ok(())
                },
            },
        );

        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains("超时"));
        assert!(stopped.get());
        assert!(restarted_old.get());
        assert_eq!(fs::read(&target).unwrap(), old_bytes);
        assert!(!target.with_extension("exe.old").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn next_startup_cleans_only_known_stale_update_artifacts() {
        let dir = unique_test_dir("startup_cleanup");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("upmc.exe");
        let unrelated = dir.join("keep.txt");
        fs::write(&target, b"MZrunning updater").unwrap();
        fs::write(target.with_extension("exe.new"), b"MZstale new").unwrap();
        fs::write(target.with_extension("exe.old"), b"MZverified backup").unwrap();
        fs::write(
            target.with_extension("exe.old.pending"),
            b"MZpartial backup",
        )
        .unwrap();
        fs::write(dir.join("upmc-update-helper-1-2.exe"), b"MZhelper").unwrap();
        let health_ack = dir.join("upmc-self-update-health-1-2.ack");
        let health_pending = dir.join("upmc-self-update-health-1-3.ack.pending");
        fs::write(&health_ack, b"stale ack").unwrap();
        fs::write(&health_pending, b"stale pending ack").unwrap();
        fs::write(&unrelated, b"keep").unwrap();

        cleanup_self_update_artifacts(&target).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"MZrunning updater");
        assert_eq!(fs::read(&unrelated).unwrap(), b"keep");
        assert!(!target.with_extension("exe.new").exists());
        assert!(!target.with_extension("exe.old").exists());
        assert!(!target.with_extension("exe.old.pending").exists());
        assert!(!dir.join("upmc-update-helper-1-2.exe").exists());
        assert!(!health_ack.exists());
        assert!(!health_pending.exists());
        fs::remove_dir_all(&dir).ok();
    }
}

/// This function owns only the staging file it creates, never a pre-existing path.
fn stage_download_reader(
    mut reader: impl Read,
    temp_path: &Path,
    info: &UpdaterVersionInfo,
    on_progress: &dyn Fn(crate::update::Progress),
) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)
        .context("创建更新器临时文件失败")?;
    let result = (|| -> Result<()> {
        let mut buf = [0u8; 65536];
        let mut downloaded = 0u64;
        loop {
            // Probe one excess byte, but never read or write an unbounded response.
            let capacity = (info.size - downloaded)
                .saturating_add(1)
                .min(buf.len() as u64) as usize;
            let count = reader
                .read(&mut buf[..capacity])
                .map_err(ArtifactReadError)
                .context("读取更新器下载数据失败")?;
            if count == 0 {
                break;
            }
            ensure!(
                count as u64 <= info.size - downloaded,
                "updater response exceeds declared size {}",
                info.size
            );
            file.write_all(&buf[..count])
                .context("写入更新器临时文件失败")?;
            downloaded += count as u64;
            on_progress(crate::update::Progress::new(
                2 + ((downloaded as f64 / info.size as f64) * 8.0) as u32,
                format!("下载更新器... {downloaded}/{} bytes", info.size),
            ));
        }
        ensure!(
            downloaded == info.size,
            "updater size mismatch: expected {}, received {downloaded}",
            info.size
        );
        file.sync_all().context("同步更新器临时文件失败")?;
        Ok(())
    })();
    drop(file);
    let result = result.and_then(|()| {
        verify_executable_identity(
            temp_path,
            &ExecutableIdentity {
                size: info.size,
                sha256: info.sha256.clone(),
            },
        )
    });
    match result {
        Ok(()) => Ok(()),
        Err(error) => match remove_stale_file(temp_path) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(crate::observability::recovery(
                error,
                Err::<(), _>(cleanup),
                temp_path.display(),
                "remove rejected staging",
            )),
        },
    }
}

#[derive(Debug)]
struct ArtifactReadError(std::io::Error);
impl std::fmt::Display for ArtifactReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}
impl std::error::Error for ArtifactReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

fn transient_network_io(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::BrokenPipe
    ) || error
        .get_ref()
        .and_then(|error| error.downcast_ref::<ureq::Error>())
        .is_some_and(transient_http_error)
}

fn transient_http_error(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::Timeout(_) | ureq::Error::StatusCode(408 | 429 | 500 | 502 | 503 | 504) => {
            true
        }
        ureq::Error::Io(error) => transient_network_io(error),
        _ => false,
    }
}

fn is_transient_network_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ureq::Error>()
        .is_some_and(transient_http_error)
        || error
            .downcast_ref::<ArtifactReadError>()
            .is_some_and(|error| transient_network_io(&error.0))
}

fn bridge_retry_with<T>(
    attempts: u32,
    delay: Duration,
    label: &str,
    mut operation: impl FnMut() -> Result<T>,
    mut sleep: impl FnMut(Duration),
) -> Result<T> {
    ensure!(attempts > 0, "retry budget must be positive");
    for attempt in 1..=attempts {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) => {
                let retry = attempt < attempts && is_transient_network_error(&error);
                crate::observability::event(
                    if retry {
                        "selfupdate.network.retry"
                    } else {
                        "selfupdate.network.failed"
                    },
                    if retry {
                        "transient network failure within finite retry budget"
                    } else {
                        "permanent failure or network retry budget exhausted"
                    },
                    format!("{error:#}"),
                    label,
                    if retry {
                        "retry same validated operation"
                    } else {
                        "abort self-update"
                    },
                    format!("attempt {attempt}/{attempts}"),
                );
                if !retry {
                    return Err(error)
                        .with_context(|| format!("{label} failed after {attempt} attempts"));
                }
                sleep(delay.saturating_mul(2u32.saturating_pow(attempt - 1)));
            }
        }
    }
    unreachable!()
}

#[cfg(test)]
#[path = "selfupdate_transfer_tests.rs"]
mod transfer_tests;

#[cfg(windows)]
mod legacy_cleanup;
#[path = "selfupdate_library.rs"]
mod library;
