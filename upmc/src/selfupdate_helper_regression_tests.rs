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
