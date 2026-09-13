// Behavioral regression tests for the UPMC-74 transaction lock.
//
// Intended 0.5.6 contract, per primary adjudication: the update transaction is
// serialized by a Windows named mutex. A transient delete-on-close
// compatibility file may exist for the duration of the transaction so an old
// 0.4.8/0.5.x opener that arrives after the new lock is held is blocked, but no
// leftover lock file may remain once the transaction ends or the process dies
// abruptly. A preopened (not yet byte-locked) legacy file still counts as an
// old holder and must block destructive cleanup.
//
// The health_* tests exercise
// `after_ack_deferred_legacy_lock_cleanup` in selfupdate.rs: ACK first, then a
// bounded, deferred, lock-only cleanup for the incoming 0.5.x library flow.
// The named_kernel_* tests use the `update_transaction_mutex_name`
// seam and prove a held transaction owns a real kernel mutex, so a
// file-lock-only implementation cannot pass.
//
// Process isolation is required for the mutual-exclusion contract, so the
// subprocess fixture re-invokes this test binary through `restart_command`
// with `--exact`. Every fixture process is killed and reaped by `Fixture`'s
// `Drop`; readiness markers are committed atomically and result claims require
// a bounded, successful child exit, so no test reads a marker while a writer is
// still running. The fixture is selected only through the environment guard so
// it can never run in the broad test suite.
use super::*;
use fs2::FileExt;
use std::cell::Cell;
use std::process::{Child, Stdio};
use std::time::Instant;

const FIXTURE_ENTRY: &str = "selfupdate::named_mutex_tests::named_mutex_fixture";
const ENV_MODE: &str = "UPMC_NAMED_MUTEX_FIXTURE_MODE";
const ENV_TARGET: &str = "UPMC_NAMED_MUTEX_FIXTURE_TARGET";
const ENV_RESULT: &str = "UPMC_NAMED_MUTEX_FIXTURE_RESULT";
const ENV_READY: &str = "UPMC_NAMED_MUTEX_FIXTURE_READY";
const ENV_ACK: &str = "UPMC_NAMED_MUTEX_FIXTURE_ACK";
const ENV_RELEASE: &str = "UPMC_NAMED_MUTEX_FIXTURE_RELEASE";
const ENV_MUTEX: &str = "UPMC_NAMED_MUTEX_FIXTURE_MUTEX";

/// `try` acquires through the new entry point; `legacy` emulates an old 0.5.x
/// opener that creates and byte-locks `exe.update.lock` directly; `die`
/// acquires through the new entry point and exits without releasing;
/// `hold-legacy-until-ack`/`hold-legacy-no-release` hold an old byte lock for
/// the health tests; `kernel-wait`/`kernel-abandon` exercise the named kernel
/// mutex; `health-poison` runs the deferred cleanup in a mutated process. Modes
/// are selected only through the environment guard.
#[test]
fn named_mutex_fixture() {
    let Ok(mode) = std::env::var(ENV_MODE) else {
        return;
    };
    let target = PathBuf::from(std::env::var_os(ENV_TARGET).expect("fixture target path"));
    match mode.as_str() {
        "try" => {
            let result = fixture_result_path();
            let acquired = acquire_update_lock(&target, 1, Duration::ZERO).is_ok();
            fs::write(&result, if acquired { "acquired" } else { "blocked" }).unwrap();
        }
        "legacy" => {
            let result = fixture_result_path();
            let legacy = target.with_extension("exe.update.lock");
            let outcome = match fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&legacy)
            {
                Ok(file) => {
                    if file.try_lock_exclusive().is_ok() {
                        "acquired"
                    } else {
                        "blocked"
                    }
                }
                Err(_) => "blocked",
            };
            fs::write(&result, outcome).unwrap();
        }
        "die" => {
            // Abrupt exit without running destructors: the OS must reclaim both
            // the named mutex and any transient delete-on-close file.
            let _held = acquire_update_lock(&target, 1, Duration::ZERO).unwrap();
            std::process::exit(0);
        }
        "hold-legacy-until-ack" => {
            let legacy = target.with_extension("exe.update.lock");
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&legacy)
                .unwrap();
            file.try_lock_exclusive().unwrap();
            commit_marker(&fixture_env_path(ENV_READY), b"locked");
            let ack = fixture_env_path(ENV_ACK);
            let deadline = Instant::now() + Duration::from_secs(30);
            while !ack.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            drop(file);
        }
        "hold-legacy-no-release" => {
            let legacy = target.with_extension("exe.update.lock");
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&legacy)
                .unwrap();
            file.try_lock_exclusive().unwrap();
            commit_marker(&fixture_env_path(ENV_READY), b"locked");
            thread::sleep(Duration::from_secs(30));
            drop(file);
        }
        "kernel-wait" => {
            use winapi::shared::winerror::WAIT_TIMEOUT;
            use winapi::um::synchapi::{OpenMutexW, WaitForSingleObject};
            use winapi::um::winbase::{WAIT_ABANDONED, WAIT_OBJECT_0};
            use winapi::um::winnt::SYNCHRONIZE;
            let result = fixture_result_path();
            let name = std::env::var(ENV_MUTEX).expect("fixture mutex name");
            let wide = wide_mutex_name(&name);
            let handle = unsafe { OpenMutexW(SYNCHRONIZE, 0, wide.as_ptr()) };
            let outcome = if handle.is_null() {
                "missing"
            } else {
                let status = unsafe { WaitForSingleObject(handle, 0) };
                unsafe { winapi::um::handleapi::CloseHandle(handle) };
                match status {
                    WAIT_OBJECT_0 => "object",
                    WAIT_ABANDONED => "abandoned",
                    WAIT_TIMEOUT => "timeout",
                    _ => "error",
                }
            };
            fs::write(&result, outcome).unwrap();
        }
        "kernel-abandon" => {
            let _held = acquire_update_lock(&target, 1, Duration::ZERO).unwrap();
            commit_marker(&fixture_env_path(ENV_READY), b"owned");
            let release = fixture_env_path(ENV_RELEASE);
            let deadline = Instant::now() + Duration::from_secs(30);
            while !release.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            std::process::exit(0);
        }
        "health-poison" => {
            library::poison_process_for_test();
            let legacy = target.with_extension("exe.update.lock");
            fs::write(&legacy, b"").unwrap();
            let acked = after_ack_deferred_legacy_lock_cleanup(
                &target,
                || true,
                3,
                Duration::from_millis(10),
            );
            let outcome = if !acked {
                "no-ack"
            } else if legacy.exists() {
                "retained"
            } else {
                "removed"
            };
            fs::write(fixture_result_path(), outcome).unwrap();
        }
        "review-mutation-race" => {
            use winapi::um::synchapi::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
            use winapi::um::winbase::{WAIT_ABANDONED, WAIT_OBJECT_0};
            // A stale empty legacy lock exists before the worker starts.
            let legacy = target.with_extension("exe.update.lock");
            fs::write(&legacy, b"").unwrap();
            let name = update_transaction_mutex_name(&target).unwrap();
            let wide = wide_mutex_name(&name);
            let raw = unsafe { CreateMutexW(std::ptr::null_mut(), 0, wide.as_ptr()) };
            assert!(
                !raw.is_null(),
                "cannot create fixture mutex {name:?}: {}",
                std::io::Error::last_os_error()
            );
            let owned = unsafe { WaitForSingleObject(raw, 0) };
            assert!(
                owned == WAIT_OBJECT_0 || owned == WAIT_ABANDONED,
                "fixture must own mutex {name:?}: {owned:#x}"
            );

            let (precheck_tx, precheck_rx) = std::sync::mpsc::channel::<()>();
            let (outcome_tx, outcome_rx) = std::sync::mpsc::channel::<String>();
            let worker_target = target.clone();
            let worker = thread::spawn(move || {
                library::ensure_process_can_update().expect("precheck must pass before poison");
                precheck_tx.send(()).unwrap();
                let outcome =
                    match acquire_update_lock(&worker_target, 100, Duration::from_millis(20)) {
                        Ok(guard) => {
                            drop(guard);
                            if worker_target.with_extension("exe.update.lock").exists() {
                                "acquired-retained"
                            } else {
                                "acquired-deleted"
                            }
                        }
                        Err(_) => {
                            if worker_target.with_extension("exe.update.lock").exists() {
                                "rejected-retained"
                            } else {
                                "rejected-deleted"
                            }
                        }
                    };
                outcome_tx.send(outcome.to_owned()).unwrap();
            });

            // Ownership and channel ordering make the interleaving deterministic:
            // the worker cannot acquire until main poisons and releases.
            precheck_rx.recv().unwrap();
            library::poison_process_for_test();
            unsafe {
                ReleaseMutex(raw);
                winapi::um::handleapi::CloseHandle(raw);
            }
            let outcome = outcome_rx.recv().unwrap();
            worker.join().unwrap();
            fs::write(fixture_result_path(), outcome).unwrap();
        }
        other => panic!("unknown named mutex fixture mode: {other}"),
    }
}

fn fixture_env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("missing fixture env {name}")))
}

fn fixture_result_path() -> PathBuf {
    fixture_env_path(ENV_RESULT)
}

/// Commit a marker with a rename so a reader can never observe a partial file.
fn commit_marker(path: &Path, value: &[u8]) {
    let pending = path.with_extension("pending");
    fs::write(&pending, value).unwrap();
    fs::rename(&pending, path).unwrap();
}

fn fixture_command() -> Command {
    let mut command = restart_command(&current_exe_path().expect("test executable path"), None);
    command
        .args(["--exact", FIXTURE_ENTRY, "--nocapture"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn wide_mutex_name(name: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(name)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

fn open_named_mutex(name: &str, access: u32) -> winapi::um::winnt::HANDLE {
    use winapi::um::synchapi::OpenMutexW;
    let wide = wide_mutex_name(name);
    unsafe { OpenMutexW(access, 0, wide.as_ptr()) }
}

fn close_handle(handle: winapi::um::winnt::HANDLE) {
    unsafe {
        winapi::um::handleapi::CloseHandle(handle);
    }
}

fn create_and_own_mutex(name: &str) -> winapi::um::winnt::HANDLE {
    use winapi::um::synchapi::{CreateMutexW, WaitForSingleObject};
    use winapi::um::winbase::{WAIT_ABANDONED, WAIT_OBJECT_0};
    let wide = wide_mutex_name(name);
    let raw = unsafe { CreateMutexW(std::ptr::null_mut(), 0, wide.as_ptr()) };
    assert!(
        !raw.is_null(),
        "cannot create fixture mutex {name:?}: {}",
        std::io::Error::last_os_error()
    );
    let owned = unsafe { WaitForSingleObject(raw, 0) };
    assert!(
        owned == WAIT_OBJECT_0 || owned == WAIT_ABANDONED,
        "fixture cannot own mutex {name:?}: {owned:#x}"
    );
    raw
}

fn release_and_close_mutex(handle: winapi::um::winnt::HANDLE) {
    use winapi::um::synchapi::ReleaseMutex;
    unsafe {
        ReleaseMutex(handle);
        winapi::um::handleapi::CloseHandle(handle);
    }
}

/// Best-effort 8.3 lookup. Returns `None` when the volume produces no distinct
/// short path; callers report that honestly rather than inventing an alias.
fn short_path(path: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use winapi::um::fileapi::GetShortPathNameW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut buffer = vec![0u16; 1024];
    let len = unsafe { GetShortPathNameW(wide.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) };
    if len == 0 || len as usize > buffer.len() {
        return None;
    }
    let short = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len as usize]));
    (short != path).then_some(short)
}

struct Fixture {
    mode: String,
    ready: PathBuf,
    result: PathBuf,
    ack: PathBuf,
    release: PathBuf,
    child: Option<Child>,
}

impl Fixture {
    fn spawn(dir: &Path, target: &Path, mode: &str) -> Self {
        Self::spawn_with(dir, target, mode, None)
    }

    fn spawn_with(dir: &Path, target: &Path, mode: &str, mutex_name: Option<&str>) -> Self {
        let tag = crate::observability::unique_id();
        let result = dir.join(format!("nm-{mode}-result-{tag}"));
        let ready = dir.join(format!("nm-{mode}-ready-{tag}"));
        let ack = dir.join(format!("nm-{mode}-ack-{tag}"));
        let release = dir.join(format!("nm-{mode}-release-{tag}"));
        let mut command = fixture_command();
        command
            .env(ENV_MODE, mode)
            .env(ENV_TARGET, target)
            .env(ENV_RESULT, &result)
            .env(ENV_READY, &ready)
            .env(ENV_ACK, &ack)
            .env(ENV_RELEASE, &release);
        if let Some(name) = mutex_name {
            command.env(ENV_MUTEX, name);
        }
        let child = command.spawn().expect("spawn named mutex fixture");
        Self {
            mode: mode.to_owned(),
            ready,
            result,
            ack,
            release,
            child: Some(child),
        }
    }

    fn wait_ready(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if self.ready.exists() {
                return;
            }
            if let Ok(Some(status)) = self.child.as_mut().unwrap().try_wait() {
                panic!("fixture {} exited before readiness: {status}", self.mode);
            }
            assert!(
                Instant::now() < deadline,
                "fixture {} readiness timed out",
                self.mode
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "fixture {} did not exit within {timeout:?}",
                self.mode
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Claims are only read after the writer has fully exited, so the parent
    /// can never observe an empty or partial result file.
    fn wait_result(&mut self, timeout: Duration) -> String {
        let status = self.wait_exit(timeout);
        assert!(
            status.success(),
            "fixture {} exited with {status} before reporting a result",
            self.mode
        );
        fs::read_to_string(&self.result)
            .unwrap_or_else(|error| panic!("fixture {} result unreadable: {error}", self.mode))
            .trim()
            .to_owned()
    }

    fn acquire_confirmed(&mut self, timeout: Duration) {
        let status = self.wait_exit(timeout);
        assert!(
            status.success(),
            "fixture {} failed to acquire and exit cleanly: {status}",
            self.mode
        );
    }

    fn release_holder(&self) {
        commit_marker(&self.release, b"release");
    }

    fn reap(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.reap();
    }
}

fn fresh_target(dir: &Path) -> PathBuf {
    let target = dir.join("upmc.exe");
    fs::write(&target, b"MZupdater fixture").unwrap();
    target
}

fn entries_dir(dir: &Path) -> Vec<String> {
    let mut entries: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    entries
}

/// A named mutex must not leave any persistent lock artifact. Transient
/// compatibility files are permitted only for the duration of the transaction;
/// a file named after the legacy lock must be gone once the guard is released.
fn lock_like(dir: &Path) -> Vec<String> {
    entries_dir(dir)
        .into_iter()
        .filter(|name| name.to_ascii_lowercase().contains("lock"))
        .collect()
}

#[test]
fn normal_lock_cycle_leaves_no_persistent_lock_file() {
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let before = entries_dir(dir.path());
    {
        let guard = acquire_update_lock(&target, 1, Duration::ZERO).unwrap();
        drop(guard);
    }
    assert_eq!(
        entries_dir(dir.path()),
        before,
        "a normal lock cycle changed the fixture contents"
    );
    assert!(
        lock_like(dir.path()).is_empty(),
        "persistent lock artifact left behind: {:?}",
        lock_like(dir.path())
    );
}

#[test]
fn fresh_cleanup_removes_stale_artifacts_without_creating_lock() {
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let staging = target.with_extension("exe.new");
    let stale_ack = dir.path().join("upmc-self-update-health-1-2.ack");
    fs::write(&staging, b"MZstale staging").unwrap();
    fs::write(&stale_ack, b"stale ack").unwrap();

    cleanup_self_update_artifacts(&target).unwrap();

    assert!(!staging.exists(), "stale staging must be removed");
    assert!(!stale_ack.exists(), "stale acknowledgment must be removed");
    assert!(
        lock_like(dir.path()).is_empty(),
        "cleanup left a persistent lock file: {:?}",
        lock_like(dir.path())
    );
}

#[test]
fn concurrent_process_is_blocked_and_release_admits_next() {
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let guard = acquire_update_lock(&target, 1, Duration::ZERO).unwrap();

    let mut blocked = Fixture::spawn(dir.path(), &target, "try");
    assert_eq!(
        blocked.wait_result(Duration::from_secs(30)),
        "blocked",
        "an independent process must not enter while the lock is held"
    );
    blocked.reap();

    drop(guard);

    let mut admitted = Fixture::spawn(dir.path(), &target, "try");
    assert_eq!(
        admitted.wait_result(Duration::from_secs(30)),
        "acquired",
        "releasing the lock must admit the next process"
    );
    admitted.reap();
}

#[test]
fn different_targets_use_independent_locks() {
    let dir = tempfile::tempdir().unwrap();
    let target_a = dir.path().join("upmc.exe");
    let target_b = dir.path().join("other-updater.exe");
    fs::write(&target_a, b"MZupdater a").unwrap();
    fs::write(&target_b, b"MZupdater b").unwrap();
    let guard = acquire_update_lock(&target_a, 1, Duration::ZERO).unwrap();

    let mut independent = Fixture::spawn(dir.path(), &target_b, "try");
    assert_eq!(
        independent.wait_result(Duration::from_secs(30)),
        "acquired",
        "a different target must not be serialized by this target's lock"
    );
    independent.reap();
    drop(guard);
}

#[test]
fn case_and_canonical_aliases_serialize_on_windows() {
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let nested = dir.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let case_alias = dir.path().join("UPMC.EXE");
    let dotted_alias = nested.join("..").join("upmc.exe");
    let guard = acquire_update_lock(&target, 1, Duration::ZERO).unwrap();

    let mut case_child = Fixture::spawn(dir.path(), &case_alias, "try");
    assert_eq!(
        case_child.wait_result(Duration::from_secs(30)),
        "blocked",
        "a case alias must serialize on the same target"
    );
    case_child.reap();

    let mut dotted_child = Fixture::spawn(dir.path(), &dotted_alias, "try");
    assert_eq!(
        dotted_child.wait_result(Duration::from_secs(30)),
        "blocked",
        "a `..` alias must serialize on the same target"
    );
    dotted_child.reap();

    drop(guard);
}

#[test]
fn lock_identity_survives_directory_entry_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let guard = acquire_update_lock(&target, 1, Duration::ZERO).unwrap();

    // Replace the directory entry, not the same file in place: a genuinely new
    // file ID must not create a second lock identity.
    let moved = dir.path().join("replaced-away.exe");
    fs::rename(&target, &moved).unwrap();
    fs::write(&target, b"MZreplacement image with a new file identity").unwrap();

    let mut child = Fixture::spawn(dir.path(), &target, "try");
    assert_eq!(
        child.wait_result(Duration::from_secs(30)),
        "blocked",
        "a new directory entry must not spawn a second lock identity"
    );
    child.reap();
    drop(guard);
}

#[test]
fn legacy_opener_arriving_after_new_acquires_is_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let guard = acquire_update_lock(&target, 1, Duration::ZERO).unwrap();

    // Simulates a 0.5.x updater that opens/creates exe.update.lock directly
    // after the new holder already checked absence and acquired.
    let mut old_opener = Fixture::spawn(dir.path(), &target, "legacy");
    assert_eq!(
        old_opener.wait_result(Duration::from_secs(30)),
        "blocked",
        "an old opener arriving after the new lock is held must be blocked"
    );
    old_opener.reap();
    drop(guard);
}

#[test]
fn killed_holder_does_not_permanently_block_later_acquisition() {
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let mut holder = Fixture::spawn(dir.path(), &target, "die");
    holder.acquire_confirmed(Duration::from_secs(30));
    holder.reap();

    let guard = acquire_update_lock(&target, 20, Duration::from_millis(25)).unwrap();
    drop(guard);

    assert!(
        lock_like(dir.path()).is_empty(),
        "an abruptly released transaction left a persistent lock file: {:?}",
        lock_like(dir.path())
    );
}

#[test]
fn abandoned_holder_does_not_authorize_backup_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("upmc.exe");
    let backup = target.with_extension("exe.old");
    fs::write(&target, b"MZcandidate").unwrap();
    fs::write(&backup, b"MZprevious").unwrap();
    fs::remove_file(&target).unwrap();

    // The transaction lock is keyed by the target path, not by an on-disk
    // image, so it must still be acquirable after a failed replacement.
    let guard = acquire_update_lock(&target, 1, Duration::ZERO);
    assert!(
        guard.is_ok(),
        "the transaction lock must not require the target file to exist"
    );
    drop(guard);

    let result = cleanup_self_update_artifacts(&target);
    assert!(
        result.is_err(),
        "a missing target cannot be treated as a completed update"
    );
    assert_eq!(
        fs::read(&backup).unwrap(),
        b"MZprevious",
        "an unverified backup must survive an abandoned transaction"
    );
}

#[test]
fn held_legacy_file_lock_blocks_destructive_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("upmc.exe");
    let backup = target.with_extension("exe.old");
    fs::write(&target, b"MZcandidate").unwrap();
    fs::write(&backup, b"MZprevious").unwrap();
    let legacy = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(target.with_extension("exe.update.lock"))
        .unwrap();
    legacy.try_lock_exclusive().unwrap();

    let result = cleanup_self_update_artifacts(&target);

    assert!(
        result.is_err(),
        "a held legacy 0.4.8/0.5.x lock must block destructive cleanup"
    );
    assert_eq!(
        fs::read(&backup).unwrap(),
        b"MZprevious",
        "original artifacts must be retained while a legacy holder exists"
    );
    drop(legacy);
}

#[test]
fn preopened_legacy_lock_blocks_destructive_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("upmc.exe");
    let backup = target.with_extension("exe.old");
    fs::write(&target, b"MZcandidate").unwrap();
    fs::write(&backup, b"MZprevious").unwrap();
    // Actual old default sharing: the old updater opens/creates the lock before
    // it byte-locks it. The new holder must treat this preopen as a live old
    // holder and refuse destructive cleanup.
    let _preopened = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(target.with_extension("exe.update.lock"))
        .unwrap();

    let result = cleanup_self_update_artifacts(&target);

    assert!(
        result.is_err(),
        "a preopened (not yet byte-locked) legacy lock must block cleanup"
    );
    assert_eq!(
        fs::read(&backup).unwrap(),
        b"MZprevious",
        "original artifacts must be retained while a legacy file is preopened"
    );
}

#[test]
fn stale_empty_legacy_lock_is_migrated_away() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("upmc.exe");
    let backup = target.with_extension("exe.old");
    fs::write(&target, b"MZcandidate").unwrap();
    fs::write(&backup, b"MZprevious").unwrap();
    let legacy = target.with_extension("exe.update.lock");
    fs::write(&legacy, b"").unwrap();

    cleanup_self_update_artifacts(&target).unwrap();

    assert!(
        !backup.exists(),
        "an unheld legacy lock must not block cleanup"
    );
    assert!(
        !legacy.exists(),
        "a stale empty legacy lock must be migrated away"
    );
    assert!(
        lock_like(dir.path()).is_empty(),
        "legacy migration must not leave a replacement lock: {:?}",
        lock_like(dir.path())
    );
}

#[test]
fn nonempty_legacy_lock_is_retained() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("upmc.exe");
    let backup = target.with_extension("exe.old");
    fs::write(&target, b"MZcandidate").unwrap();
    fs::write(&backup, b"MZprevious").unwrap();
    let legacy = target.with_extension("exe.update.lock");
    fs::write(&legacy, b"someone else's data").unwrap();

    let _ = cleanup_self_update_artifacts(&target);

    assert_eq!(
        fs::read(&legacy).unwrap(),
        b"someone else's data",
        "a nonempty legacy lock must not be deleted or rewritten"
    );
}

#[test]
fn legacy_lock_directory_is_retained() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("upmc.exe");
    let backup = target.with_extension("exe.old");
    fs::write(&target, b"MZcandidate").unwrap();
    fs::write(&backup, b"MZprevious").unwrap();
    let legacy = target.with_extension("exe.update.lock");
    fs::create_dir(&legacy).unwrap();

    let result = cleanup_self_update_artifacts(&target);

    assert!(
        result.is_err(),
        "a directory at the legacy lock path must not authorize cleanup"
    );
    assert!(
        legacy.is_dir(),
        "cleanup deleted a directory at the lock path"
    );
    assert!(backup.exists(), "original artifacts must be retained");
}

#[test]
fn legacy_lock_reparse_point_is_not_followed_or_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("upmc.exe");
    let backup = target.with_extension("exe.old");
    fs::write(&target, b"MZcandidate").unwrap();
    fs::write(&backup, b"MZprevious").unwrap();
    let real = dir.path().join("real-legacy-lock-target");
    fs::write(&real, b"held elsewhere").unwrap();
    let legacy = target.with_extension("exe.update.lock");
    match std::os::windows::fs::symlink_file(&real, &legacy) {
        Ok(()) => {}
        // ERROR_PRIVILEGE_NOT_HELD: creating a file symlink needs either
        // developer mode or SeCreateSymbolicLinkPrivilege. Skip rather than
        // fail the suite on a machine that cannot express the fixture.
        Err(error) if error.raw_os_error() == Some(1314) => {
            eprintln!("skipping reparse-point test: symlink privilege unavailable: {error}");
            return;
        }
        Err(error) => panic!("cannot create reparse fixture: {error}"),
    }

    let result = cleanup_self_update_artifacts(&target);

    assert!(
        result.is_err(),
        "a reparse point at the legacy lock path must not authorize cleanup"
    );
    assert!(
        fs::symlink_metadata(&legacy).is_ok(),
        "the reparse-point lock path was deleted"
    );
    assert!(real.exists(), "the reparse target was deleted");
}

#[test]
fn health_ack_without_legacy_leaves_no_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let acked = Cell::new(false);
    let sent = after_ack_deferred_legacy_lock_cleanup(
        &target,
        || {
            acked.set(true);
            true
        },
        5,
        Duration::from_millis(10),
    );
    assert!(sent, "a successful acknowledgment must be reported as sent");
    assert!(acked.get(), "acknowledgment callback must run");
    assert!(
        !target.with_extension("exe.update.lock").exists(),
        "an acknowledged target without a legacy lock must stay clean"
    );
    assert!(lock_like(dir.path()).is_empty());
}

#[test]
fn health_failed_ack_does_not_touch_old_files() {
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let backup = target.with_extension("exe.old");
    let staging = target.with_extension("exe.new");
    let helper = dir.path().join("upmc-update-helper-health.exe");
    let legacy = target.with_extension("exe.update.lock");
    fs::write(&backup, b"MZprevious").unwrap();
    fs::write(&staging, b"MZstaged").unwrap();
    fs::write(&helper, b"MZhelper").unwrap();
    fs::write(&legacy, b"").unwrap();

    let acked = Cell::new(false);
    let sent = after_ack_deferred_legacy_lock_cleanup(
        &target,
        || {
            acked.set(true);
            false
        },
        5,
        Duration::from_millis(10),
    );

    assert!(
        !sent,
        "a failed acknowledgment must not be reported as sent"
    );
    assert!(acked.get(), "acknowledgment callback must still run");
    assert_eq!(fs::read(&legacy).unwrap(), b"");
    assert_eq!(fs::read(&backup).unwrap(), b"MZprevious");
    assert_eq!(fs::read(&staging).unwrap(), b"MZstaged");
    assert_eq!(fs::read(&helper).unwrap(), b"MZhelper");
}

#[test]
fn health_deferred_cleanup_removes_stale_legacy_after_ack() {
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let backup = target.with_extension("exe.old");
    let staging = target.with_extension("exe.new");
    let helper = dir.path().join("upmc-update-helper-health.exe");
    fs::write(&backup, b"MZprevious").unwrap();
    fs::write(&staging, b"MZstaged").unwrap();
    fs::write(&helper, b"MZhelper").unwrap();

    let mut holder = Fixture::spawn(dir.path(), &target, "hold-legacy-until-ack");
    holder.wait_ready(Duration::from_secs(30));
    let ack_signal = holder.ack.clone();
    let acked = Cell::new(false);
    let sent = after_ack_deferred_legacy_lock_cleanup(
        &target,
        || {
            acked.set(true);
            commit_marker(&ack_signal, b"ack");
            true
        },
        200,
        Duration::from_millis(25),
    );

    assert!(sent, "ACK must be sent without waiting for the old holder");
    assert!(
        acked.get(),
        "acknowledgment callback must run before cleanup"
    );
    holder.acquire_confirmed(Duration::from_secs(30));
    holder.reap();

    assert!(
        !target.with_extension("exe.update.lock").exists(),
        "the stale legacy lock must be removed after ACK and holder release"
    );
    assert_eq!(fs::read(&backup).unwrap(), b"MZprevious");
    assert_eq!(fs::read(&staging).unwrap(), b"MZstaged");
    assert_eq!(fs::read(&helper).unwrap(), b"MZhelper");
}

#[test]
fn health_unreleased_holder_keeps_lock_but_ack_still_sent() {
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let backup = target.with_extension("exe.old");
    fs::write(&backup, b"MZprevious").unwrap();
    let mut holder = Fixture::spawn(dir.path(), &target, "hold-legacy-no-release");
    holder.wait_ready(Duration::from_secs(30));

    let acked = Cell::new(false);
    let sent = after_ack_deferred_legacy_lock_cleanup(
        &target,
        || {
            acked.set(true);
            true
        },
        3,
        Duration::from_millis(10),
    );

    assert!(
        sent,
        "an already-sent ACK must not be suppressed by a bounded cleanup failure"
    );
    assert!(acked.get());
    assert!(
        target.with_extension("exe.update.lock").exists(),
        "a still-held legacy lock must be retained"
    );
    assert_eq!(fs::read(&backup).unwrap(), b"MZprevious");
    holder.reap();
}

#[test]
fn health_invalid_nonempty_legacy_is_retained() {
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let legacy = target.with_extension("exe.update.lock");
    fs::write(&legacy, b"held by an unknown owner").unwrap();

    let sent =
        after_ack_deferred_legacy_lock_cleanup(&target, || true, 3, Duration::from_millis(10));

    assert!(sent, "ACK must still be sent");
    assert_eq!(
        fs::read(&legacy).unwrap(),
        b"held by an unknown owner",
        "a nonempty legacy item must never be consumed as a stale lock"
    );
}

#[test]
fn health_invalid_reparse_legacy_is_retained() {
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let real = dir.path().join("real-legacy-owner");
    fs::write(&real, b"held elsewhere").unwrap();
    let legacy = target.with_extension("exe.update.lock");
    match std::os::windows::fs::symlink_file(&real, &legacy) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(1314) => {
            eprintln!("skipping reparse health test: symlink privilege unavailable: {error}");
            return;
        }
        Err(error) => panic!("cannot create reparse fixture: {error}"),
    }

    let sent =
        after_ack_deferred_legacy_lock_cleanup(&target, || true, 3, Duration::from_millis(10));

    assert!(sent, "ACK must still be sent");
    assert!(
        fs::symlink_metadata(&legacy).is_ok(),
        "a reparse legacy item must be retained"
    );
    assert!(real.exists(), "the reparse target must be retained");
}

#[test]
fn health_deferred_cleanup_respects_process_mutation_guard() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("upmc.exe");
    let mut child = Fixture::spawn(dir.path(), &target, "health-poison");
    assert_eq!(
        child.wait_result(Duration::from_secs(30)),
        "retained",
        "a mutated process must not consume the legacy lock"
    );
    child.reap();
}

#[test]
fn named_kernel_held_transaction_owns_named_mutex() {
    use winapi::um::winnt::SYNCHRONIZE;
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let name = update_transaction_mutex_name(&target).unwrap();
    let guard = acquire_update_lock(&target, 1, Duration::ZERO).unwrap();

    let handle = open_named_mutex(&name, SYNCHRONIZE);
    assert!(
        !handle.is_null(),
        "a held transaction must own a real named kernel mutex {name:?}; a file-lock-only implementation fails here"
    );
    close_handle(handle);
    drop(guard);
}

#[test]
fn named_kernel_child_cannot_own_held_mutex() {
    use winapi::um::winnt::SYNCHRONIZE;
    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let name = update_transaction_mutex_name(&target).unwrap();
    let guard = acquire_update_lock(&target, 1, Duration::ZERO).unwrap();
    let handle = open_named_mutex(&name, SYNCHRONIZE);
    assert!(
        !handle.is_null(),
        "the parent must own the named mutex before contention"
    );
    close_handle(handle);

    let mut contender = Fixture::spawn_with(dir.path(), &target, "kernel-wait", Some(&name));
    assert_eq!(
        contender.wait_result(Duration::from_secs(30)),
        "timeout",
        "a second owner must not enter while the kernel mutex is held"
    );
    contender.reap();
    drop(guard);
}

#[test]
fn named_kernel_abandoned_owner_reports_wait_abandoned() {
    use winapi::um::synchapi::{ReleaseMutex, WaitForSingleObject};
    use winapi::um::winbase::WAIT_ABANDONED;
    use winapi::um::winnt::SYNCHRONIZE;
    // winapi 0.3.9 does not export MUTEX_MODIFY_STATE; this is the documented
    // Windows access bit (winnt.h) needed only for ReleaseMutex.
    const MUTEX_MODIFY_STATE: u32 = 0x0001;

    let dir = tempfile::tempdir().unwrap();
    let target = fresh_target(dir.path());
    let name = update_transaction_mutex_name(&target).unwrap();

    let mut owner = Fixture::spawn(dir.path(), &target, "kernel-abandon");
    owner.wait_ready(Duration::from_secs(30));
    // Open while the child still owns the object and retain the handle, so the
    // object survives the child exit and can report abandonment.
    let handle = open_named_mutex(&name, SYNCHRONIZE | MUTEX_MODIFY_STATE);
    assert!(
        !handle.is_null(),
        "a live owner must expose the named kernel mutex {name:?}"
    );
    owner.release_holder();
    owner.acquire_confirmed(Duration::from_secs(30));
    owner.reap();

    let status = unsafe { WaitForSingleObject(handle, 0) };
    unsafe {
        ReleaseMutex(handle);
    }
    close_handle(handle);
    assert_eq!(
        status, WAIT_ABANDONED,
        "an abruptly exited owner must report WAIT_ABANDONED, was {status:#x}"
    );
}

#[test]
fn review_unicode_basename_case_aliases_share_mutex_name() {
    let dir = tempfile::tempdir().unwrap();
    let lower = dir.path().join("updater-é.exe");
    fs::write(&lower, b"MZupdater").unwrap();
    let upper = dir.path().join("UPDATER-É.EXE");

    let lower_name = update_transaction_mutex_name(&lower).unwrap();
    let upper_name = update_transaction_mutex_name(&upper).unwrap();
    assert_eq!(
        lower_name, upper_name,
        "Unicode case aliases of one directory entry must share one transaction identity"
    );
}

#[test]
fn review_unicode_case_alias_process_is_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let lower = dir.path().join("updater-é.exe");
    fs::write(&lower, b"MZupdater").unwrap();
    let upper = dir.path().join("UPDATER-É.EXE");

    let holder = create_and_own_mutex(&update_transaction_mutex_name(&lower).unwrap());
    let mut child = Fixture::spawn(dir.path(), &upper, "try");
    assert_eq!(
        child.wait_result(Duration::from_secs(30)),
        "blocked",
        "a Unicode case alias must not enter while the long-name mutex is held"
    );
    child.reap();
    release_and_close_mutex(holder);
}

#[test]
fn review_short_name_alias_shares_mutex_name() {
    let dir = tempfile::tempdir().unwrap();
    let long = dir.path().join("updater-long-basename-for-8.3.exe");
    fs::write(&long, b"MZupdater").unwrap();
    let Some(short) = short_path(&long) else {
        eprintln!(
            "skipping 8.3 seam test: volume produced no distinct short path for {}",
            long.display()
        );
        return;
    };

    assert_eq!(
        fs::canonicalize(&long).unwrap(),
        fs::canonicalize(&short).unwrap(),
        "the 8.3 alias must resolve to the same directory entry"
    );
    assert_eq!(
        update_transaction_mutex_name(&long).unwrap(),
        update_transaction_mutex_name(&short).unwrap(),
        "an 8.3 short basename must share the long-name transaction identity"
    );
}

#[test]
fn review_short_name_alias_process_is_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let long = dir.path().join("updater-long-basename-for-8.3.exe");
    fs::write(&long, b"MZupdater").unwrap();
    let Some(short) = short_path(&long) else {
        eprintln!(
            "skipping 8.3 exclusion test: volume produced no distinct short path for {}",
            long.display()
        );
        return;
    };

    let holder = create_and_own_mutex(&update_transaction_mutex_name(&long).unwrap());
    let mut child = Fixture::spawn(dir.path(), &short, "try");
    assert_eq!(
        child.wait_result(Duration::from_secs(30)),
        "blocked",
        "an 8.3 alias must not enter while the long-name mutex is held"
    );
    child.reap();
    release_and_close_mutex(holder);
}

#[test]
fn review_process_mutation_between_precheck_and_legacy_acquire() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("upmc.exe");
    let mut child = Fixture::spawn(dir.path(), &target, "review-mutation-race");
    assert_eq!(
        child.wait_result(Duration::from_secs(30)),
        "rejected-retained",
        "process mutation after the precheck must reject acquisition before any legacy open/create/delete"
    );
    child.reap();
}
