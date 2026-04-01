use std::path::Path;

use anyhow::{Context, Result};

use crate::{ProxyConfig, discord};

const DWRITE_DLL: &str = "DWrite.dll";
const FORCE_PROXY_DLL: &str = "force-proxy.dll";
const PROXY_TXT: &str = "proxy.txt";

/// Install proxy files into a specific Discord app directory.
pub fn install_to_dir(
    dir: &Path,
    proxy_dll: &[u8],
    force_proxy_dll: &[u8],
    config: &ProxyConfig,
) -> Result<()> {
    std::fs::write(dir.join(DWRITE_DLL), proxy_dll)
        .with_context(|| format!("Failed to write {DWRITE_DLL} to {}", dir.display()))?;
    std::fs::write(dir.join(FORCE_PROXY_DLL), force_proxy_dll)
        .with_context(|| format!("Failed to write {FORCE_PROXY_DLL} to {}", dir.display()))?;
    std::fs::write(dir.join(PROXY_TXT), config.to_proxy_txt())
        .with_context(|| format!("Failed to write {PROXY_TXT} to {}", dir.display()))?;
    Ok(())
}

/// Install proxy to **all** Discord app directories.
///
/// This is a key improvement over the original C# installer which only
/// installed to the latest version. Installing to all directories helps
/// survive Discord auto-updates (fix for issue #26).
pub fn install(proxy_dll: &[u8], force_proxy_dll: &[u8], config: &ProxyConfig) -> Result<()> {
    let dirs = discord::get_app_dirs()?;
    anyhow::ensure!(!dirs.is_empty(), "No Discord app directories found");

    for dir in &dirs {
        install_to_dir(dir, proxy_dll, force_proxy_dll, config)?;
    }
    Ok(())
}

/// Kill Discord, install proxy to all directories, then relaunch.
pub fn install_and_run(
    proxy_dll: &[u8],
    force_proxy_dll: &[u8],
    config: &ProxyConfig,
) -> Result<()> {
    discord::kill()?;
    install(proxy_dll, force_proxy_dll, config)?;
    discord::launch()
}

/// Check if proxy is installed in the latest app directory.
pub fn is_installed() -> Result<bool> {
    let dir = discord::get_latest_app_dir()?;
    Ok(dir.join(DWRITE_DLL).exists())
}

/// Uninstall proxy from **all** Discord app directories.
pub fn uninstall() -> Result<()> {
    discord::kill()?;
    let dirs = discord::get_app_dirs()?;
    for dir in &dirs {
        remove_from_dir(dir);
    }
    Ok(())
}

/// Ensure proxy files exist in every `app-*` directory.
///
/// This is the primary fix for
/// [issue #26](https://github.com/runetfreedom/discord-voice-proxy/issues/26):
/// when Discord auto-updates it creates a new `app-X.Y.Z` directory without
/// proxy files. Call this function periodically (e.g. from an updater like
/// UPMC) to repair any directory that is missing the proxy.
pub fn ensure_installed(
    proxy_dll: &[u8],
    force_proxy_dll: &[u8],
    config: &ProxyConfig,
) -> Result<()> {
    let dirs = discord::get_app_dirs()?;
    for dir in &dirs {
        let missing_dll = !dir.join(DWRITE_DLL).exists();
        let missing_fp = !dir.join(FORCE_PROXY_DLL).exists();
        if missing_dll || missing_fp {
            install_to_dir(dir, proxy_dll, force_proxy_dll, config)?;
        }
    }
    Ok(())
}

/// Update only the proxy configuration (proxy.txt) in all app directories,
/// without re-writing the DLL files.
pub fn update_config(config: &ProxyConfig) -> Result<()> {
    let dirs = discord::get_app_dirs()?;
    for dir in &dirs {
        if dir.join(DWRITE_DLL).exists() {
            std::fs::write(dir.join(PROXY_TXT), config.to_proxy_txt())
                .with_context(|| format!("Failed to write {PROXY_TXT} to {}", dir.display()))?;
        }
    }
    Ok(())
}

fn remove_from_dir(dir: &Path) {
    for name in [DWRITE_DLL, FORCE_PROXY_DLL, PROXY_TXT] {
        let path = dir.join(name);
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }
}
