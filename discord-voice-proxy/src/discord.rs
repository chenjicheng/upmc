use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// Get the root Discord directory (`%LocalAppData%/Discord`).
pub fn get_root_dir() -> Result<PathBuf> {
    let local_app_data =
        std::env::var("LOCALAPPDATA").context("LOCALAPPDATA environment variable not set")?;
    let root = PathBuf::from(local_app_data).join("Discord");
    if !root.is_dir() {
        bail!("Discord directory not found: {}", root.display());
    }
    Ok(root)
}

/// Get all `app-*` directories sorted by version (oldest first).
pub fn get_app_dirs() -> Result<Vec<PathBuf>> {
    let root = get_root_dir()?;
    let mut dirs = Vec::new();

    for entry in std::fs::read_dir(&root).context("Failed to read Discord directory")? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("app-") {
            dirs.push(entry.path());
        }
    }

    dirs.sort_by(|a, b| parse_version(a).cmp(&parse_version(b)));
    Ok(dirs)
}

/// Get the latest Discord app directory.
pub fn get_latest_app_dir() -> Result<PathBuf> {
    get_app_dirs()?
        .into_iter()
        .last()
        .context("No Discord app-* directory found")
}

/// Check if Discord is installed.
pub fn is_installed() -> bool {
    get_latest_app_dir().is_ok()
}

/// Kill all running Discord processes.
pub fn kill() -> Result<()> {
    let _ = std::process::Command::new("taskkill")
        .args(["/f", "/im", "Discord.exe"])
        .output();
    std::thread::sleep(std::time::Duration::from_secs(1));
    Ok(())
}

/// Launch Discord from the latest app directory.
pub fn launch() -> Result<()> {
    let app_dir = get_latest_app_dir()?;
    let exe = app_dir.join("Discord.exe");
    std::process::Command::new(&exe)
        .current_dir(&app_dir)
        .spawn()
        .with_context(|| format!("Failed to launch {}", exe.display()))?;
    Ok(())
}

/// Detect common proxy clients and return `(name, host, port)` if found.
pub fn detect_proxy_client() -> Option<(&'static str, &'static str, u16)> {
    let processes: Vec<String> = list_process_names();

    if processes.iter().any(|p| p == "v2rayn") {
        return Some(("v2rayN", "127.0.0.1", 10808));
    }
    if processes.iter().any(|p| p == "nekoray" || p == "nekobox") {
        return Some(("NekoRay/NekoBox", "127.0.0.1", 2080));
    }
    if processes
        .iter()
        .any(|p| p == "invisible man xray" || p == "invisible-man-xray")
    {
        return Some(("Invisible Man - XRay", "127.0.0.1", 10801));
    }

    None
}

fn parse_version(path: &PathBuf) -> Vec<u32> {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("app-"))
        .map(|v| v.split('.').filter_map(|s| s.parse().ok()).collect())
        .unwrap_or_default()
}

fn list_process_names() -> Vec<String> {
    let output = std::process::Command::new("tasklist")
        .args(["/fo", "csv", "/nh"])
        .output()
        .ok();

    let Some(output) = output else {
        return Vec::new();
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let name = line.split(',').next()?;
            let name = name.trim_matches('"').trim();
            Some(
                name.strip_suffix(".exe")
                    .unwrap_or(name)
                    .to_lowercase(),
            )
        })
        .collect()
}
