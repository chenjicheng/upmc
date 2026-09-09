use super::*;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

#[derive(PartialEq, Eq)]
struct FileIdentity {
    volume: u32,
    index: u64,
    size: u64,
    sha256: String,
}
impl FileIdentity {
    fn read(path: &Path) -> Result<Self> {
        use winapi::um::fileapi::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
        ensure!(
            fs::symlink_metadata(path)?.file_type().is_file(),
            "cleanup requires a regular file"
        );
        let file = fs::File::open(path)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } != 0,
            "cannot capture cleanup file identity: {}",
            std::io::Error::last_os_error()
        );
        let content = executable_identity(path)?;
        Ok(Self {
            volume: info.dwVolumeSerialNumber,
            index: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
            size: content.size,
            sha256: content.sha256,
        })
    }
    fn same_content(&self, other: &Self) -> bool {
        self.size == other.size && self.sha256 == other.sha256
    }
}

struct Proof {
    target: PathBuf,
    helper: PathBuf,
    installed: FileIdentity,
    backup: FileIdentity,
    helper_identity: FileIdentity,
}
impl Proof {
    fn capture(target: &Path, helper: &Path) -> Result<Self> {
        let target = fs::canonicalize(target)?;
        let helper = fs::canonicalize(helper)?;
        ensure!(
            target != helper && target.parent() == helper.parent(),
            "legacy helper must be a separate same-directory image"
        );
        ensure!(
            helper
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_helper_file_name),
            "unrecognized legacy helper"
        );
        let installed = FileIdentity::read(&target)?;
        let backup = FileIdentity::read(&target.with_extension("exe.old"))?;
        let helper_identity = FileIdentity::read(&helper)?;
        ensure!(
            helper_identity.same_content(&backup),
            "legacy helper differs from incoming rollback backup"
        );
        Ok(Self {
            target,
            helper,
            installed,
            backup,
            helper_identity,
        })
    }
    fn cleanup(&self, successful: bool) -> Result<()> {
        ensure!(successful, "legacy parent did not complete successfully");
        let _lock = acquire_update_lock(&self.target, 1, Duration::ZERO)?;
        // Staging and changed file identities invalidate an incoming proof.
        // Never reuse startup's broad directory cleanup here.
        for path in [
            self.target.with_extension("exe.new"),
            self.target.with_extension("exe.old.pending"),
        ] {
            match fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
                Ok(_) => bail!("new update staging exists: {}", path.display()),
            }
        }
        for entry in fs::read_dir(self.target.parent().context("target directory missing")?)? {
            let path = entry?.path();
            ensure!(
                path == self.helper
                    || !path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(is_helper_file_name),
                "another helper invalidates incoming cleanup proof"
            );
        }
        ensure!(
            FileIdentity::read(&self.target)? == self.installed,
            "installed executable changed"
        );
        let backup = self.target.with_extension("exe.old");
        ensure!(
            FileIdentity::read(&backup)? == self.backup,
            "rollback backup changed"
        );
        ensure!(
            FileIdentity::read(&self.helper)? == self.helper_identity,
            "helper image changed"
        );
        // Removing the exact departed image first also catches an unexpected live
        // image section; never discard the backup if that deletion is denied.
        fs::remove_file(&self.helper).context("legacy helper remains locked")?;
        fs::remove_file(&backup).context("cannot remove validated legacy backup")?;
        Ok(())
    }
}

fn report(error: anyhow::Error) {
    crate::observability::event(
        "selfupdate.legacy_cleanup_retained",
        "legacy cleanup proof unavailable or invalidated",
        format!("{error:#}"),
        "incoming legacy transaction",
        "retain artifacts; health acknowledgment remains independent",
        "post-health cleanup",
    );
}

pub(super) fn after_ack<T>(
    capture: Result<Option<T>>,
    ack: impl FnOnce() -> bool,
    cleanup: impl FnOnce(T) -> Result<()>,
) {
    let proof = match capture {
        Ok(proof) => proof,
        Err(error) => {
            report(error);
            None
        }
    };
    if ack()
        && let Some(proof) = proof
        && let Err(error) = cleanup(proof)
    {
        report(error);
    }
}

pub(super) struct ParentProof {
    handle: OwnedHandle,
    files: Proof,
}
impl ParentProof {
    pub(super) fn capture() -> Result<Option<Self>> {
        use std::os::windows::ffi::OsStringExt;
        use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcess};
        use winapi::um::winbase::QueryFullProcessImageNameW;
        use winapi::um::winnt::{PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZE};
        let parent = parent_pid()?;
        let raw =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, parent) };
        ensure!(
            !raw.is_null(),
            "cannot open actual parent: {}",
            std::io::Error::last_os_error()
        );
        // Own this handle through waiting and cleanup. Never reopen by PID.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
        let mut buffer = vec![0u16; 32768];
        let mut len = buffer.len() as u32;
        ensure!(
            unsafe { QueryFullProcessImageNameW(raw, 0, buffer.as_mut_ptr(), &mut len) } != 0,
            "cannot query parent image: {}",
            std::io::Error::last_os_error()
        );
        let image = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len as usize]));
        // self_update/self_replace parents and ordinary launches are not the
        // legacy protocol. Their files remain exclusively library-owned.
        if !image
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_helper_file_name)
        {
            return Ok(None);
        }
        ensure!(
            creation_time(raw)? < creation_time(unsafe { GetCurrentProcess() })?,
            "parent PID was recycled or has invalid creation order"
        );
        let files = Proof::capture(&std::env::current_exe()?, &image)?;
        Ok(Some(Self { handle, files }))
    }
    pub(super) fn finish(self) -> Result<()> {
        let successful = process_exited_successfully(&self.handle, 120_000)?;
        self.files.cleanup(successful)
    }
}

fn creation_time(handle: winapi::um::winnt::HANDLE) -> Result<u64> {
    use winapi::shared::minwindef::FILETIME;
    use winapi::um::processthreadsapi::GetProcessTimes;
    let mut created: FILETIME = unsafe { std::mem::zeroed() };
    let mut exited: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } != 0,
        "cannot query process creation: {}",
        std::io::Error::last_os_error()
    );
    Ok(((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64)
}

fn parent_pid() -> Result<u32> {
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::tlhelp32::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    ensure!(
        raw != INVALID_HANDLE_VALUE,
        "cannot snapshot process ancestry: {}",
        std::io::Error::last_os_error()
    );
    let _snapshot = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut available = unsafe { Process32FirstW(raw, &mut entry) };
    while available != 0 {
        if entry.th32ProcessID == std::process::id() {
            return Ok(entry.th32ParentProcessID);
        }
        available = unsafe { Process32NextW(raw, &mut entry) };
    }
    bail!("current process missing from ancestry snapshot")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("upmc.exe");
        let helper = dir.path().join("upmc-update-helper-legacy.exe");
        fs::write(&target, b"MZnew").unwrap();
        fs::write(target.with_extension("exe.old"), b"MZold").unwrap();
        fs::write(&helper, b"MZold").unwrap();
        (dir, target, helper)
    }
    #[test]
    fn success_removes_only_proven_files() {
        let (_dir, target, helper) = fixture();
        let unrelated = target.with_extension("keep");
        fs::write(&unrelated, b"keep").unwrap();
        Proof::capture(&target, &helper)
            .unwrap()
            .cleanup(true)
            .unwrap();
        assert!(!target.with_extension("exe.old").exists());
        assert!(!helper.exists());
        assert!(target.exists() && unrelated.exists());
        assert!(target.with_extension("exe.update.lock").exists());
    }
    #[test]
    fn wrong_helper_is_rejected() {
        let (_dir, target, helper) = fixture();
        fs::write(&helper, b"MZwrong").unwrap();
        assert!(Proof::capture(&target, &helper).is_err());
    }
    #[test]
    fn identical_replacement_backup_and_unrecognized_helper_are_rejected() {
        let (_dir, target, helper) = fixture();
        let proof = Proof::capture(&target, &helper).unwrap();
        let backup = target.with_extension("exe.old");
        fs::rename(&backup, target.with_extension("saved")).unwrap();
        fs::write(&backup, b"MZold").unwrap();
        assert!(proof.cleanup(true).is_err());
        assert!(backup.exists() && helper.exists());
        let wrong = helper.with_file_name("ordinary.exe");
        fs::rename(&helper, &wrong).unwrap();
        assert!(Proof::capture(&target, &wrong).is_err());
    }
    #[test]
    fn failed_exit_changed_files_new_staging_and_lock_retain_backup() {
        for case in 0..7 {
            let (_dir, target, helper) = fixture();
            let proof = Proof::capture(&target, &helper).unwrap();
            let mut lock = None;
            match case {
                1 => fs::write(target.with_extension("exe.old"), b"MZchanged").unwrap(),
                2 => fs::write(&target, b"MZchanged").unwrap(),
                3 => fs::write(target.with_extension("exe.new"), b"MZstage").unwrap(),
                4 => fs::write(target.with_extension("exe.old.pending"), b"MZpending").unwrap(),
                5 => {
                    lock = Some(acquire_update_lock(&target, 1, Duration::ZERO).unwrap());
                }
                6 => fs::write(
                    target
                        .parent()
                        .unwrap()
                        .join("upmc-update-helper-other.exe"),
                    b"MZother",
                )
                .unwrap(),
                _ => {}
            }
            assert!(proof.cleanup(case != 0).is_err(), "case {case}");
            assert!(target.with_extension("exe.old").exists() && helper.exists());
            drop(lock);
        }
    }
}

#[cfg(test)]
mod ack_tests {
    use super::*;
    #[test]
    fn capture_failure_does_not_prevent_ack_and_failed_ack_never_cleans() {
        let acked = std::cell::Cell::new(false);
        after_ack::<()>(
            Err(anyhow::anyhow!("capture failed")),
            || {
                acked.set(true);
                true
            },
            |_| panic!("no proof"),
        );
        assert!(acked.get());
        after_ack(Ok(Some(())), || false, |_| panic!("ack failed"));
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use std::os::windows::process::CommandExt;
    const ENTRY: &str = "selfupdate::legacy_cleanup::native_tests::native_fixture_entry";
    fn command(path: &Path) -> Command {
        let mut cmd = Command::new(path);
        cmd.args(["--exact", ENTRY, "--nocapture"])
            .creation_flags(0x08000000);
        cmd
    }
    #[test]
    fn native_fixture_entry() {
        let Ok(mode) = std::env::var("UPMC_LEGACY_FIXTURE_MODE") else {
            return;
        };
        let target = PathBuf::from(std::env::var_os("UPMC_LEGACY_FIXTURE_TARGET").unwrap());
        if mode == "helper" {
            let _child = command(&target)
                .env("UPMC_LEGACY_FIXTURE_MODE", "candidate")
                .spawn()
                .unwrap();
            let start = std::time::Instant::now();
            while !target.with_extension("ready").exists() {
                assert!(
                    start.elapsed() < Duration::from_secs(25),
                    "candidate did not capture parent"
                );
                thread::sleep(Duration::from_millis(20));
            }
            std::process::exit(
                std::env::var("UPMC_LEGACY_FIXTURE_EXIT")
                    .unwrap()
                    .parse()
                    .unwrap(),
            );
        }
        let outcome = (|| -> Result<()> {
            let proof = ParentProof::capture()?.context("parent was not recognized")?;
            // The parent must still be alive here: it waits for this marker.
            ensure!(
                !process_exited_successfully(&proof.handle, 0)?,
                "live parent considered complete"
            );
            ensure!(
                proof.files.cleanup(true).is_err(),
                "live helper image was deleted"
            );
            ensure!(
                target.with_extension("exe.old").exists(),
                "live helper lost its backup"
            );
            fs::write(target.with_extension("ready"), b"ack")?;
            proof.finish()
        })();
        fs::write(
            target.with_extension("result.pending"),
            if outcome.is_ok() {
                "cleaned".to_owned()
            } else {
                format!("retained: {outcome:?}")
            },
        )
        .unwrap();
        fs::rename(
            target.with_extension("result.pending"),
            target.with_extension("result"),
        )
        .unwrap();
    }
    #[test]
    fn native_parent_handle_success_and_failure() {
        for exit in [0, 7] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("candidate.exe");
            let helper = dir.path().join("upmc-update-helper-native.exe");
            let exe = std::env::current_exe().unwrap();
            for path in [&target, &helper, &target.with_extension("exe.old")] {
                fs::copy(&exe, path).unwrap();
            }
            let mut parent = command(&helper)
                .env("UPMC_LEGACY_FIXTURE_MODE", "helper")
                .env("UPMC_LEGACY_FIXTURE_TARGET", &target)
                .env("UPMC_LEGACY_FIXTURE_EXIT", exit.to_string())
                .spawn()
                .unwrap();
            let restricted = unsafe {
                winapi::um::processthreadsapi::OpenProcess(
                    winapi::um::winnt::SYNCHRONIZE,
                    0,
                    parent.id(),
                )
            };
            assert!(!restricted.is_null());
            let restricted = unsafe { OwnedHandle::from_raw_handle(restricted.cast()) };
            let start = std::time::Instant::now();
            while !target.with_extension("result").exists() {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = parent.kill();
                    panic!("native fixture timeout");
                }
                thread::sleep(Duration::from_millis(25));
            }
            let result = fs::read_to_string(target.with_extension("result")).unwrap();
            if !target.with_extension("ready").exists() {
                let _ = parent.kill();
            }
            let _ = parent.wait();
            assert!(
                process_exited_successfully(&restricted, 0).is_err(),
                "exit-query failure must retain artifacts"
            );
            if exit == 0 {
                assert_eq!(result, "cleaned");
            } else {
                assert!(result.starts_with("retained:"), "{result}");
            }
            assert_eq!(target.with_extension("exe.old").exists(), exit != 0);
            assert_eq!(helper.exists(), exit != 0);
        }
    }
    #[test]
    fn unrelated_native_parent_is_ignored() {
        assert!(ParentProof::capture().unwrap().is_none());
    }
}
fn process_exited_successfully(handle: &OwnedHandle, timeout: u32) -> Result<bool> {
    use winapi::shared::winerror::WAIT_TIMEOUT;
    use winapi::um::processthreadsapi::GetExitCodeProcess;
    use winapi::um::synchapi::WaitForSingleObject;
    use winapi::um::winbase::WAIT_OBJECT_0;
    match unsafe { WaitForSingleObject(handle.as_raw_handle().cast(), timeout) } {
        WAIT_TIMEOUT => Ok(false),
        WAIT_OBJECT_0 => {
            let mut code = 0;
            ensure!(
                unsafe { GetExitCodeProcess(handle.as_raw_handle().cast(), &mut code) } != 0,
                "cannot query captured parent exit: {}",
                std::io::Error::last_os_error()
            );
            Ok(code == 0)
        }
        _ => bail!(
            "cannot wait for captured parent: {}",
            std::io::Error::last_os_error()
        ),
    }
}

#[cfg(test)]
mod health_gate_tests {
    use super::*;
    #[test]
    fn health_result_requires_committed_ack() {
        let dir = tempfile::tempdir().unwrap();
        let ack = StartupHealthAck {
            path: dir.path().join("health.ack"),
            token: "0".repeat(64),
        };
        assert!(acknowledge_health_when_window_ready_with(
            &ack,
            1,
            Duration::ZERO,
            || Ok(true),
            |_| {}
        ));
        let invalid = StartupHealthAck {
            path: dir.path().join("missing").join("health.ack"),
            token: "0".repeat(64),
        };
        assert!(!acknowledge_health_when_window_ready_with(
            &invalid,
            1,
            Duration::ZERO,
            || Ok(true),
            |_| {}
        ));
    }
}
