use std::path::PathBuf;

use anyhow::{bail, Context, Result};

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
    kill_with(|program, args| {
        use std::os::windows::process::CommandExt;
        std::process::Command::new(program)
            .args(args)
            .creation_flags(0x08000000)
            .output()
    })
}

fn kill_with(
    mut run: impl FnMut(&str, &[&str]) -> std::io::Result<std::process::Output>,
) -> Result<()> {
    let stopped = run("taskkill", &["/f", "/im", "Discord.exe"])
        .context("Failed to execute Discord stop command")?;
    let processes =
        run("tasklist", &["/fo", "csv", "/nh"]).context("Failed to verify Discord termination")?;
    let still_running = running_from_output(&processes)?;
    anyhow::ensure!(
        !still_running,
        "Discord is still running after stop ({}): {}",
        stopped.status,
        String::from_utf8_lossy(&stopped.stderr)
    );
    // A failed taskkill may mean Discord was already closed. A successful
    // process snapshot proving absence is the authoritative postcondition.
    Ok(())
}

/// Query process state without launching or stopping Discord.
pub fn is_running() -> Result<bool> {
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new("tasklist")
        .args(["/fo", "csv", "/nh"])
        .creation_flags(0x08000000)
        .output()
        .context("Failed to query Discord process state")?;
    running_from_output(&output)
}

fn running_from_output(processes: &std::process::Output) -> Result<bool> {
    anyhow::ensure!(
        processes.status.success(),
        "Failed to verify Discord termination: {}",
        String::from_utf8_lossy(&processes.stderr)
    );
    let listing = String::from_utf8_lossy(&processes.stdout);
    anyhow::ensure!(
        !listing.trim().is_empty(),
        "Empty process listing while verifying Discord termination"
    );
    Ok(listing.lines().any(|line| {
        line.split(',')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"')
            .eq_ignore_ascii_case("Discord.exe")
    }))
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
            Some(name.strip_suffix(".exe").unwrap_or(name).to_lowercase())
        })
        .collect()
}

#[cfg(test)]
mod stop_tests {
    use super::*;
    fn output(code: u32, stdout: &str) -> std::process::Output {
        use std::os::windows::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"fixture diagnostic".to_vec(),
        }
    }
    #[test]
    fn stop_must_prove_discord_absent_and_report_command_failures() {
        assert!(kill_with(|_, _| Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied"
        )))
        .is_err());
        assert!(kill_with(|program, _| Ok(if program == "taskkill" {
            output(1, "")
        } else {
            output(0, "\"Discord.exe\",\"123\"")
        }))
        .is_err());
        assert!(kill_with(|program, _| Ok(if program == "taskkill" {
            output(0, "")
        } else {
            output(1, "")
        }))
        .is_err());
        assert!(kill_with(|program, _| Ok(if program == "taskkill" {
            output(128, "")
        } else {
            output(0, "\"explorer.exe\",\"123\"")
        }))
        .is_ok());
    }
}
