//! Named-mutex transaction lock with a temporary legacy compatibility guard.
//!
//! New updaters serialize on one Windows named mutex per target. Because
//! 0.4.8-0.5.5 processes only know the persistent `exe.update.lock` file,
//! acquisition also holds an exclusive handle on that path for the whole
//! transaction so an old process observes an open, delete-pending lock. The file
//! is never removed by pathname: a newly created file is delete-on-close and an
//! existing empty regular file is marked delete-pending on the validated
//! handle. A normal cycle therefore leaves no persistent lock file.
use super::*;
use sha2::{Digest, Sha256};
use std::marker::PhantomData;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use winapi::shared::winerror::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION,
    WAIT_TIMEOUT,
};
use winapi::um::fileapi::{
    BY_HANDLE_FILE_INFORMATION, FILE_DISPOSITION_INFO, GetFileInformationByHandle,
    SetFileInformationByHandle,
};
use winapi::um::minwinbase::FileDispositionInfo;
use winapi::um::synchapi::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use winapi::um::winbase::{
    FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAG_OPEN_REPARSE_POINT, WAIT_ABANDONED, WAIT_FAILED,
    WAIT_OBJECT_0,
};
use winapi::um::winnt::{
    DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, GENERIC_READ, GENERIC_WRITE,
};

/// RAII ownership of the per-target named mutex and the transient legacy guard.
///
/// Win32 mutex ownership is thread-affine, so the guard is deliberately
/// `!Send + !Sync`; every value is acquired and released on one thread.
pub(super) struct TransactionGuard {
    legacy: Option<fs::File>,
    mutex: OwnedHandle,
    _thread_affine: PhantomData<*mut ()>,
}

impl Drop for TransactionGuard {
    fn drop(&mut self) {
        // Close the legacy guard before releasing the mutex so the next
        // acquirer can never observe our exclusive delete-pending file after
        // the mutex has changed hands.
        self.legacy.take();
        if unsafe { ReleaseMutex(self.mutex.as_raw_handle()) } == 0 {
            crate::observability::event(
                "selfupdate.lock.release_failed",
                "cannot release named update mutex",
                std::io::Error::last_os_error(),
                "named update mutex",
                "handle closes on drop",
                std::process::id(),
            );
        }
    }
}

/// Acquire the per-target transaction lock or fail closed.
///
/// The named mutex is taken first; two new processes therefore never contend on
/// the legacy file. Once ownership exists as RAII, the process mutation guard is
/// rechecked before the legacy file is opened, created, or marked for deletion.
pub(super) fn acquire(target: &Path, attempts: usize, delay: Duration) -> Result<TransactionGuard> {
    let normalized = normalized_target(target)?;
    let name = mutex_name_of(&normalized);
    let mutex = create_mutex(&name)?;
    wait_for_ownership(target, &mutex, attempts, delay)?;
    let mut guard = TransactionGuard {
        legacy: None,
        mutex,
        _thread_affine: PhantomData,
    };
    // Ownership now exists as RAII, so a rejected mutation guard drops this
    // guard and releases the mutex without touching the legacy file.
    library::ensure_process_can_update()?;
    guard.legacy = Some(acquire_legacy_guard(&normalized, attempts, delay)?);
    Ok(guard)
}

/// Resolve the stable path identity used by both the mutex and legacy lock.
///
/// An existing target is canonicalized in full so case-only, 8.3 short-name,
/// `..`, and parent-junction aliases of one directory entry share one identity.
/// A temporarily absent target (replacement or recovery) falls back to the
/// canonical parent plus the given basename, so identity never requires the
/// file to exist and never keys on a file ID. Errors other than a missing
/// target fail closed.
fn normalized_target(target: &Path) -> Result<PathBuf> {
    match fs::canonicalize(target) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = target
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .with_context(|| format!("自更新目标缺少父目录: {}", target.display()))?;
            let basename = target
                .file_name()
                .with_context(|| format!("自更新目标缺少文件名: {}", target.display()))?;
            let parent = fs::canonicalize(parent)
                .with_context(|| format!("无法解析自更新目标目录: {}", parent.display()))?;
            Ok(parent.join(basename))
        }
        Err(error) => {
            Err(error).with_context(|| format!("无法解析自更新目标: {}", target.display()))
        }
    }
}

/// Stable mutex name for a target after full-path normalization.
#[cfg(test)]
pub(super) fn mutex_name(target: &Path) -> Result<String> {
    Ok(mutex_name_of(&normalized_target(target)?))
}

fn mutex_name_of(normalized: &Path) -> String {
    let mut hasher = Sha256::new();
    for unit in normalized.as_os_str().encode_wide() {
        hasher.update(ascii_lower(unit).to_le_bytes());
    }
    let mut name = String::from("Global\\UPMC.SelfUpdate.");
    use std::fmt::Write as _;
    for byte in hasher.finalize() {
        write!(name, "{byte:02x}").expect("writing to a String cannot fail");
    }
    name
}

fn ascii_lower(unit: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&unit) {
        unit + u16::from(b'a' - b'A')
    } else {
        unit
    }
}

fn create_mutex(name: &str) -> Result<OwnedHandle> {
    let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // A NULL security descriptor yields the default DACL derived from the
    // current process token and a non-inheritable handle; no Everyone ACE is
    // ever installed. Any failure fails the transaction closed.
    let raw = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
    ensure!(
        !raw.is_null(),
        "cannot create named update mutex: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

fn wait_for_ownership(
    target: &Path,
    mutex: &OwnedHandle,
    attempts: usize,
    delay: Duration,
) -> Result<()> {
    ensure!(
        attempts > 0,
        "named update mutex attempt budget must be positive"
    );
    for attempt in 1..=attempts {
        match unsafe { WaitForSingleObject(mutex.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => return Ok(()),
            WAIT_ABANDONED => {
                crate::observability::event(
                    "selfupdate.lock.abandoned",
                    "previous owner exited without releasing the named update mutex",
                    "WAIT_ABANDONED",
                    target.display(),
                    "continue; every artifact is revalidated independently",
                    format!("attempt {attempt} of {attempts}"),
                );
                return Ok(());
            }
            WAIT_TIMEOUT => {
                let retrying = attempt < attempts;
                crate::observability::event(
                    if retrying {
                        "selfupdate.lock.retry"
                    } else {
                        "selfupdate.lock.failed"
                    },
                    if retrying {
                        "named update mutex busy; bounded retry"
                    } else {
                        "named update mutex busy; attempt budget exhausted"
                    },
                    "WAIT_TIMEOUT",
                    target.display(),
                    if retrying {
                        format!(
                            "attempt {} of {attempts}; delay {}ms",
                            attempt + 1,
                            delay.as_millis()
                        )
                    } else {
                        format!("attempt {attempt} of {attempts}; return failure")
                    },
                    target.display(),
                );
                if !retrying {
                    bail!(
                        "timed out acquiring named update mutex for {}",
                        target.display()
                    );
                }
                thread::sleep(delay);
            }
            WAIT_FAILED => bail!(
                "cannot wait for named update mutex: {}",
                std::io::Error::last_os_error()
            ),
            other => bail!("unexpected named update mutex wait result: {other:#x}"),
        }
    }
    unreachable!("positive attempt budget returns on success or its last failure")
}

fn acquire_legacy_guard(target: &Path, attempts: usize, delay: Duration) -> Result<fs::File> {
    let path = target.with_extension("exe.update.lock");
    ensure!(
        attempts > 0,
        "legacy update lock attempt budget must be positive"
    );
    let mut contention: Option<std::io::Error> = None;
    for attempt in 1..=attempts {
        match open_legacy_existing(&path) {
            Ok(file) => {
                validate_legacy_handle(&path, &file)?;
                mark_legacy_delete_pending(&path, &file)?;
                return Ok(file);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match create_legacy_exclusive(&path) {
                    Ok(file) => {
                        validate_legacy_handle(&path, &file)?;
                        return Ok(file);
                    }
                    Err(error) if is_already_exists(&error) => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("cannot create legacy update lock: {}", path.display())
                        });
                    }
                }
            }
            // A live old opener denies sharing and a terminated one may still be
            // finishing its delete-pending close, so only these contention errors
            // are retried. A non-file item at the path is permanent and stops
            // immediately; the handle-bound validation still governs deletion.
            Err(error) if is_retryable_legacy_open(&error) && !is_permanent_legacy_item(&path) => {
                contention = Some(error);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("legacy update lock is inaccessible: {}", path.display())
                });
            }
        }
        if attempt < attempts {
            thread::sleep(delay);
        }
    }
    match contention {
        Some(error) => Err(error).with_context(|| {
            format!(
                "legacy 0.4.8-0.5.5 update lock is held or inaccessible: {}",
                path.display()
            )
        }),
        None => bail!(
            "legacy update lock create/open race did not settle: {}",
            path.display()
        ),
    }
}

fn is_permanent_legacy_item(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Ok(metadata) if !metadata.file_type().is_file()
    )
}

fn is_retryable_legacy_open(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == ERROR_SHARING_VIOLATION as i32
                || code == ERROR_LOCK_VIOLATION as i32
                || code == ERROR_ACCESS_DENIED as i32
    )
}

/// OPEN_EXISTING with no sharing: fails if any old process has the file open,
/// even before it has acquired its byte-range lock.
fn open_legacy_existing(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// CREATE_NEW: atomic create; delete-on-close guarantees no leftover on exit.
fn create_legacy_exclusive(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .share_mode(0)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
        .open(path)
}

fn is_already_exists(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::AlreadyExists
        || error.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32)
}

/// Validation is bound to the exclusive handle, not the path.
fn validate_legacy_handle(path: &Path, file: &fs::File) -> Result<()> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
        "cannot inspect legacy update lock {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    ensure!(
        info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "legacy update lock is a reparse point; refusing to delete: {}",
        path.display()
    );
    ensure!(
        info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0,
        "legacy update lock is a directory; refusing to delete: {}",
        path.display()
    );
    let size = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
    ensure!(
        size == 0,
        "legacy update lock is not empty; refusing to delete: {}",
        path.display()
    );
    Ok(())
}

/// Classic delete-pending on the validated handle; never a pathname unlink.
fn mark_legacy_delete_pending(path: &Path, file: &fs::File) -> Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    ensure!(
        unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&info as *const FILE_DISPOSITION_INFO).cast_mut().cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } != 0,
        "cannot mark legacy update lock delete-pending {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    Ok(())
}
