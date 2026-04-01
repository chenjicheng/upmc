//! DWrite.dll proxy — drop-in replacement that:
//!
//! 1. Forwards the real `DWriteCreateFactory` export to the system DWrite.dll
//! 2. Reads `proxy.txt` and sets SOCKS5 environment variables
//! 3. Loads `force-proxy.dll` (which intercepts network calls)
//! 4. Monitors for new Discord `app-*` directories and copies proxy files there
//!    (fix for <https://github.com/runetfreedom/discord-voice-proxy/issues/26>)
//!
//! Build with `cargo build -p discord-proxy-dll --release`.
//! The output `dwrite.dll` should be renamed/installed as `DWrite.dll` in the
//! Discord app directory.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Environment::SetEnvironmentVariableW;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MessageBoxW};

// ── Types ──────────────────────────────────────────────────────────────────

type DWriteCreateFactoryFn =
    unsafe extern "system" fn(u32, *const c_void, *mut *mut c_void) -> i32;

struct ProxyState {
    original_fn: DWriteCreateFactoryFn,
    // Keep the handle alive so the real DLL is not unloaded.
    _original_dll: isize,
}

// Safety: the function pointer and module handle are process-global and
// immutable after initialisation.
unsafe impl Send for ProxyState {}
unsafe impl Sync for ProxyState {}

// ── Global state ───────────────────────────────────────────────────────────

static STATE: OnceLock<ProxyState> = OnceLock::new();

fn get_state() -> &'static ProxyState {
    STATE.get_or_init(|| unsafe { initialize() })
}

// ── Exported function ──────────────────────────────────────────────────────

/// Forwarded to the **real** `DWrite.dll` in System32.
///
/// Discord (or any process loading us as DWrite.dll) calls this transparently.
#[no_mangle]
pub unsafe extern "system" fn DWriteCreateFactory(
    factory_type: u32,
    iid: *const c_void,
    factory: *mut *mut c_void,
) -> i32 {
    let state = get_state();
    (state.original_fn)(factory_type, iid, factory)
}

// ── Initialisation ─────────────────────────────────────────────────────────

unsafe fn initialize() -> ProxyState {
    // 1. Load real DWrite.dll from System32
    let mut sys_path = [0u16; 260];
    let len = GetSystemDirectoryW(sys_path.as_mut_ptr(), 260) as usize;

    let suffix = to_wide("\\DWrite.dll");
    core::ptr::copy_nonoverlapping(suffix.as_ptr(), sys_path.as_mut_ptr().add(len), suffix.len());

    let original_dll = LoadLibraryW(sys_path.as_ptr());
    if original_dll == 0 {
        show_error("Cannot load original DWrite.dll from System32");
        std::process::exit(1);
    }

    // GetProcAddress always takes an ANSI name
    let proc = GetProcAddress(original_dll, b"DWriteCreateFactory\0".as_ptr());
    let Some(proc) = proc else {
        show_error("DWriteCreateFactory not found in real DWrite.dll");
        std::process::exit(1);
    };
    let original_fn: DWriteCreateFactoryFn = core::mem::transmute(proc);

    // 2. Read proxy.txt → set env vars → load force-proxy.dll
    load_force_proxy();

    // 3. Background thread: copy proxy files into new app-* dirs (issue #26)
    std::thread::spawn(monitor_discord_updates);

    ProxyState {
        original_fn,
        _original_dll: original_dll,
    }
}

// ── Force-proxy loader ─────────────────────────────────────────────────────

/// Read `proxy.txt` next to the running exe, set the key=value pairs as
/// environment variables, then load `force-proxy.dll`.
unsafe fn load_force_proxy() {
    let Some(dir) = exe_dir() else { return };

    // Set env vars from proxy.txt
    let proxy_path = dir.join("proxy.txt");
    if let Ok(content) = std::fs::read_to_string(&proxy_path) {
        for line in content.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key_w = to_wide(key.trim());
            let value_w = to_wide(value.trim());
            SetEnvironmentVariableW(key_w.as_ptr(), value_w.as_ptr());
        }
    }

    // Load force-proxy.dll (does the actual SOCKS5 interception)
    let dll_path = dir.join("force-proxy.dll");
    let dll_path_w = to_wide(&dll_path.to_string_lossy());
    let handle = LoadLibraryW(dll_path_w.as_ptr());
    if handle == 0 {
        let code = GetLastError();
        show_error(&format!(
            "Cannot load force-proxy.dll\nPath: {}\nError code: {code}",
            dll_path.display()
        ));
    }
}

// ── Update monitor (issue #26 fix) ────────────────────────────────────────

/// Periodically scan the Discord root for new `app-*` directories and copy
/// the three proxy files (`DWrite.dll`, `force-proxy.dll`, `proxy.txt`) into
/// any directory that is missing them.
///
/// This runs as long as the host process (Discord) is alive.  It replaces
/// the original project's `CreateProcessW` hook with a simpler, more
/// reliable mechanism that also handles the case where Discord updates while
/// completely closed and then relaunches from `Update.exe`.
fn monitor_discord_updates() {
    let Some(current_dir) = exe_dir() else { return };
    let Some(discord_root) = current_dir.parent().map(|p| p.to_path_buf()) else {
        return;
    };

    let files = ["DWrite.dll", "force-proxy.dll", "proxy.txt"];

    loop {
        std::thread::sleep(std::time::Duration::from_secs(5));

        let Ok(entries) = std::fs::read_dir(&discord_root) else {
            continue;
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with("app-") {
                continue;
            }

            let target_dir = entry.path();

            // Skip the directory we are already running from.
            if target_dir == current_dir {
                continue;
            }

            // Only touch directories that contain Discord.exe (a valid install).
            if !target_dir.join("Discord.exe").exists() {
                continue;
            }

            for file_name in &files {
                let src = current_dir.join(file_name);
                let dst = target_dir.join(file_name);
                // 直接尝试复制，忽略 NotFound/AlreadyExists 等错误（避免 TOCTOU）
                if !dst.exists() {
                    let _ = std::fs::copy(&src, &dst);
                }
            }
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

unsafe fn show_error(msg: &str) {
    let msg_w = to_wide(msg);
    let title_w = to_wide("Discord Voice Proxy");
    MessageBoxW(0, msg_w.as_ptr(), title_w.as_ptr(), MB_ICONERROR);
}
