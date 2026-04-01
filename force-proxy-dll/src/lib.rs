#![allow(non_snake_case, unused)]

mod hooks;
mod socks5;

/// Called by DWrite.dll (or any loader) via `LoadLibrary`.
/// Initialization happens in DllMain → DLL_PROCESS_ATTACH.
#[no_mangle]
unsafe extern "system" fn DllMain(
    _h_module: windows_sys::Win32::Foundation::HMODULE,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> windows_sys::Win32::Foundation::BOOL {
    const DLL_PROCESS_ATTACH: u32 = 1;
    const DLL_PROCESS_DETACH: u32 = 0;

    match reason {
        DLL_PROCESS_ATTACH => {
            hooks::init();
            1 // TRUE
        }
        DLL_PROCESS_DETACH => {
            hooks::destroy();
            1
        }
        _ => 1,
    }
}
