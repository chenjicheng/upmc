use std::path::Path;

use anyhow::{Context, Result};

use crate::{discord, ProxyConfig};

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
    ensure_in_dirs(
        &dirs,
        proxy_dll,
        force_proxy_dll,
        config,
        discord::is_running,
        discord::kill,
        discord::launch,
    )
}

fn ensure_in_dirs(
    dirs: &[std::path::PathBuf],
    proxy_dll: &[u8],
    force_proxy_dll: &[u8],
    config: &ProxyConfig,
    running: impl FnOnce() -> Result<bool>,
    stop: impl FnOnce() -> Result<()>,
    start: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let text = config.to_proxy_txt();
    let mut changes = Vec::new();
    for dir in dirs {
        let missing_dll = !dir.join(DWRITE_DLL).exists();
        let missing_fp = !dir.join(FORCE_PROXY_DLL).exists();
        let changed = match std::fs::read_to_string(dir.join(PROXY_TXT)) {
            Ok(existing) => existing != text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to read {PROXY_TXT} in {}", dir.display()))
            }
        };
        changes.push((dir, missing_dll || missing_fp, changed));
    }
    // Discord consumes these environment settings at process startup.
    // Reload only when an installed proxy's configuration actually changes.
    let reload = changes
        .iter()
        .any(|(dir, _, changed)| *changed && dir.join(DWRITE_DLL).exists());
    let reload =
        reload && running().context("Failed to query Discord before configuration update")?;
    if reload {
        stop().context("Failed to stop Discord for proxy configuration update")?;
    }
    for (dir, missing, changed) in changes {
        if missing {
            install_to_dir(dir, proxy_dll, force_proxy_dll, config)?;
        } else if changed {
            std::fs::write(dir.join(PROXY_TXT), &text)
                .with_context(|| format!("Failed to write {PROXY_TXT} to {}", dir.display()))?;
        }
    }
    if reload {
        start().context("Failed to restart Discord after proxy configuration update")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_udp_config_is_updated_and_reloaded_only_when_changed() {
        let root = std::env::temp_dir().join(format!(
            "upmc-udp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut config = ProxyConfig {
            address: "127.0.0.1".into(),
            port: 10808,
            login: None,
            password: None,
            udp: false,
        };
        install_to_dir(&root, b"dll", b"force", &config).unwrap();
        config.udp = true;
        let calls = std::cell::RefCell::new(Vec::new());
        ensure_in_dirs(
            &[root.clone()],
            b"dll",
            b"force",
            &config,
            || Ok(true),
            || {
                calls.borrow_mut().push("stop");
                Ok(())
            },
            || {
                assert!(std::fs::read_to_string(root.join(PROXY_TXT))
                    .unwrap()
                    .contains("SOCKS5_PROXY_UDP=true"));
                calls.borrow_mut().push("start");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["stop", "start"]);
        assert_eq!(std::fs::read(root.join(DWRITE_DLL)).unwrap(), b"dll");
        ensure_in_dirs(
            &[root.clone()],
            b"dll",
            b"force",
            &config,
            || Ok(true),
            || panic!("unchanged stop"),
            || panic!("unchanged start"),
        )
        .unwrap();
        config.udp = false;
        ensure_in_dirs(
            &[root.clone()],
            b"dll",
            b"force",
            &config,
            || Ok(false),
            || panic!("closed Discord must stay closed"),
            || panic!("closed Discord must not launch"),
        )
        .unwrap();
        assert!(std::fs::read_to_string(root.join(PROXY_TXT))
            .unwrap()
            .contains("SOCKS5_PROXY_UDP=false"));
        config.udp = true;
        install_to_dir(&root, b"dll", b"force", &config).unwrap();
        config.udp = false;
        let error = ensure_in_dirs(
            &[root.clone()],
            b"dll",
            b"force",
            &config,
            || Ok(true),
            || anyhow::bail!("stop denied"),
            || panic!("start after stop failure"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("stop denied"));
        assert!(std::fs::read_to_string(root.join(PROXY_TXT))
            .unwrap()
            .contains("SOCKS5_PROXY_UDP=true"));
        let error = ensure_in_dirs(
            &[root.clone()],
            b"dll",
            b"force",
            &config,
            || Ok(true),
            || {
                let mut attrs = std::fs::metadata(root.join(PROXY_TXT))?.permissions();
                attrs.set_readonly(true);
                std::fs::set_permissions(root.join(PROXY_TXT), attrs)?;
                Ok(())
            },
            || panic!("start after failed write"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("Failed to write proxy.txt"));
        let mut attrs = std::fs::metadata(root.join(PROXY_TXT))
            .unwrap()
            .permissions();
        attrs.set_readonly(false);
        std::fs::set_permissions(root.join(PROXY_TXT), attrs).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
