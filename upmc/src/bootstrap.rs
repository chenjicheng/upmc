// ============================================================
// bootstrap.rs — 首次运行自举模块
// ============================================================

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::config;
use crate::retry;
use crate::update::Progress;
use crate::version::Downloads;

pub fn needs_bootstrap(base_dir: &Path) -> bool {
    let checks = [
        config::PCL2_EXE,
        config::PACKWIZ_BOOTSTRAP_JAR,
        config::FABRIC_INSTALLER_JAR,
    ];

    checks.iter().any(|path| !base_dir.join(path).exists())
}

pub fn is_bootstrapped(base_dir: &Path) -> bool {
    base_dir.join(config::PCL2_EXE).exists()
        && base_dir.join(config::LOCAL_VERSION_FILE).exists()
}

pub fn run_bootstrap(
    base_dir: &Path,
    downloads: &Downloads,
    on_progress: &dyn Fn(Progress),
) -> Result<()> {
    on_progress(Progress::new(2, "正在创建目录结构..."));
    let dirs = [
        ".minecraft",
        ".minecraft/mods",
        ".minecraft/config",
        "updater",
    ];
    for dir in &dirs {
        fs::create_dir_all(base_dir.join(dir))
            .with_context(|| format!("创建目录失败: {dir}"))?;
    }

    let pcl2_path = base_dir.join(config::PCL2_EXE);
    if !pcl2_path.exists() {
        let pcl2_url = downloads
            .pcl2_url
            .as_deref()
            .context("server.json 中未配置 PCL2 下载地址 (downloads.pcl2_url)")?;
        let pcl2_sha256 = require_download_sha(downloads.pcl2_sha256.as_deref(), "pcl2_sha256")?;

        on_progress(Progress::new(31, "正在下载启动器..."));
        download_file_verified(pcl2_url, &pcl2_path, pcl2_sha256, on_progress, 31, 38)?;
    }
    on_progress(Progress::new(38, "启动器就绪"));

    let packwiz_jar = base_dir.join(config::PACKWIZ_BOOTSTRAP_JAR);
    if !packwiz_jar.exists() {
        let packwiz_url = downloads
            .packwiz_bootstrap_url
            .as_deref()
            .context("server.json 中未配置 packwiz 下载地址 (downloads.packwiz_bootstrap_url)")?;
        let packwiz_sha256 = require_download_sha(
            downloads.packwiz_bootstrap_sha256.as_deref(),
            "packwiz_bootstrap_sha256",
        )?;

        on_progress(Progress::new(39, "正在下载模组同步器..."));
        download_file_verified(packwiz_url, &packwiz_jar, packwiz_sha256, on_progress, 39, 42)?;
    }
    on_progress(Progress::new(42, "模组同步器就绪"));

    let fabric_jar = base_dir.join(config::FABRIC_INSTALLER_JAR);
    if !fabric_jar.exists() {
        let fabric_url = downloads
            .fabric_installer_url
            .as_deref()
            .context("server.json 中未配置 Fabric 安装器下载地址 (downloads.fabric_installer_url)")?;
        let fabric_sha256 = require_download_sha(
            downloads.fabric_installer_sha256.as_deref(),
            "fabric_installer_sha256",
        )?;

        on_progress(Progress::new(43, "正在下载 Fabric 安装器..."));
        download_file_verified(fabric_url, &fabric_jar, fabric_sha256, on_progress, 43, 46)?;
    }
    on_progress(Progress::new(46, "Fabric 安装器就绪"));

    let setup_ini = base_dir.join(config::PCL2_SETUP_INI_PATH);
    if !setup_ini.exists() {
        on_progress(Progress::new(47, "正在配置启动器..."));
        fs::write(&setup_ini, config::PCL2_SETUP_INI)
            .context("写入 Setup.ini 失败")?;
    }

    let settings_marker = base_dir.join("updater/.settings_installed");
    if !settings_marker.exists() {
        if let Some(ref settings_url) = downloads.settings_url {
            let settings_sha256 =
                require_download_sha(downloads.settings_sha256.as_deref(), "settings_sha256")?;

            on_progress(Progress::new(48, "正在下载默认设置..."));
            let zip_path = base_dir.join("updater/settings-download.zip");
            download_file_verified(settings_url, &zip_path, settings_sha256, on_progress, 48, 49)?;

            on_progress(Progress::new(49, "正在应用默认设置..."));
            let mc_dir = base_dir.join(config::MINECRAFT_DIR);
            fs::create_dir_all(&mc_dir).context("创建 .minecraft 目录失败")?;
            extract_settings_zip(&zip_path, &mc_dir)
                .context("解压设置包失败")?;

            fs::remove_file(&zip_path).ok();
        }
        fs::write(&settings_marker, "installed")
            .context("写入设置安装标记失败")?;
    }

    on_progress(Progress::new(50, "首次安装完成"));
    Ok(())
}

pub(crate) fn download_file(
    url: &str,
    dest: &Path,
    on_progress: &dyn Fn(Progress),
    progress_start: u32,
    progress_end: u32,
) -> Result<()> {
    validate_download_url(url)?;
    let url_owned = url.to_string();
    let dest_owned = dest.to_path_buf();

    retry::with_retry(
        config::RETRY_MAX_ATTEMPTS,
        config::RETRY_BASE_DELAY_SECS,
        &format!("下载 {}", url),
        || {
            download_file_inner(
                &url_owned,
                &dest_owned,
                on_progress,
                progress_start,
                progress_end,
            )
        },
    )
}

fn download_file_verified(
    url: &str,
    dest: &Path,
    expected_sha256: &str,
    on_progress: &dyn Fn(Progress),
    progress_start: u32,
    progress_end: u32,
) -> Result<()> {
    validate_sha256_hex(expected_sha256)
        .with_context(|| format!("无效的 SHA256 配置: {}", dest.display()))?;

    download_file(url, dest, on_progress, progress_start, progress_end)?;
    verify_sha256(dest, expected_sha256)
        .with_context(|| format!("文件 SHA256 校验失败: {}", dest.display()))?;
    Ok(())
}

fn download_file_inner(
    url: &str,
    dest: &Path,
    on_progress: &dyn Fn(Progress),
    progress_start: u32,
    progress_end: u32,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let agent = config::download_agent();

    let response = agent
        .get(url)
        .call()
        .with_context(|| format!("下载失败: {url}"))?;

    let total_size = response.body().content_length().unwrap_or(0);

    let mut reader = response.into_body().into_reader();
    let mut file = fs::File::create(dest)
        .with_context(|| format!("创建文件失败: {}", dest.display()))?;

    let mut buf = [0u8; 65536];
    let mut downloaded: u64 = 0;

    loop {
        let n = reader.read(&mut buf).context("读取下载数据失败")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("写入文件失败")?;
        downloaded += n as u64;

        if total_size > 0 {
            let fraction = downloaded as f64 / total_size as f64;
            let pct = progress_start
                + (fraction * (progress_end - progress_start) as f64) as u32;
            let mb_done = downloaded as f64 / 1_048_576.0;
            let mb_total = total_size as f64 / 1_048_576.0;
            on_progress(Progress::new(
                pct.min(progress_end),
                format!("下载中... {mb_done:.1}/{mb_total:.1} MB"),
            ));
        }
    }

    drop(file);
    validate_downloaded_file(dest)?;
    Ok(())
}

fn validate_download_url(url: &str) -> Result<()> {
    if !url.starts_with("https://") {
        bail!("下载 URL 必须使用 HTTPS 协议: {url}");
    }

    let host = extract_url_host(url).context("无法解析下载 URL 主机名")?;
    let allowed = config::TRUSTED_DOWNLOAD_HOST_SUFFIXES.iter().any(|suffix| {
        host == *suffix || host.ends_with(&format!(".{suffix}"))
    });

    if !allowed {
        bail!("下载 URL 主机不在信任列表中: {host}");
    }
    Ok(())
}

fn extract_url_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host_port = rest.split(['/', '?', '#']).next()?;
    let host = host_port.split(':').next()?.trim().to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

fn validate_downloaded_file(path: &Path) -> Result<()> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let expected_magic: &[u8] = match ext.as_str() {
        "exe" => b"MZ",
        "jar" | "zip" => b"PK",
        _ => return Ok(()),
    };

    let mut f = fs::File::open(path)
        .with_context(|| format!("打开下载文件失败: {}", path.display()))?;
    let mut magic = vec![0u8; expected_magic.len()];
    if f.read_exact(&mut magic).is_err() || magic != expected_magic {
        let _ = fs::remove_file(path);
        bail!(
            "下载的文件格式无效（可能是代理返回了错误页面）: {}\n\
             请检查网络连接或代理服务是否正常后重试。",
            path.display()
        );
    }

    Ok(())
}

fn require_download_sha<'a>(value: Option<&'a str>, field_name: &str) -> Result<&'a str> {
    let value = value.with_context(|| format!("server.json 中未配置下载校验值 downloads.{field_name}"))?;
    validate_sha256_hex(value)
        .with_context(|| format!("server.json 中 downloads.{field_name} 不是有效 SHA256"))?;
    Ok(value)
}

fn validate_sha256_hex(value: &str) -> Result<()> {
    let is_valid = value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit());
    if !is_valid {
        bail!("SHA256 必须是 64 位十六进制字符串");
    }
    Ok(())
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("读取文件失败: {}", path.display()))?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != expected.to_ascii_lowercase() {
        let _ = fs::remove_file(path);
        bail!("期望 {expected}，实际 {actual}");
    }
    Ok(())
}

fn safe_zip_output_path(dest: &Path, entry_name: &str) -> Result<PathBuf> {
    let relative = Path::new(entry_name);
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::Prefix(_) | Component::RootDir
        )
    }) {
        bail!("ZIP 条目包含非法路径: {entry_name}");
    }

    Ok(dest.join(relative))
}

pub(crate) fn extract_zip(zip_path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(zip_path)
        .with_context(|| format!("打开 ZIP 失败: {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("读取 ZIP 失败")?;
    fs::create_dir_all(dest)?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if name.is_empty() || entry.is_dir() {
            continue;
        }
        let out_path = safe_zip_output_path(dest, &name)?;
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut outfile = fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut outfile)?;
    }
    Ok(())
}

fn extract_settings_zip(zip_path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(zip_path)
        .with_context(|| format!("打开设置包 ZIP 失败: {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("读取设置包 ZIP 失败")?;

    fs::create_dir_all(dest)?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("读取设置包条目 #{i} 失败"))?;

        let name = entry.name().to_string();
        if name.is_empty() {
            continue;
        }

        let out_path = safe_zip_output_path(dest, &name)?;

        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
        } else {
            if out_path.exists() {
                continue;
            }
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut outfile = fs::File::create(&out_path)
                .with_context(|| format!("创建设置文件失败: {}", out_path.display()))?;
            std::io::copy(&mut entry, &mut outfile)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn validate_valid_exe() {
        let dir = std::env::temp_dir().join("upmc_test_validate_exe");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.exe");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(b"MZ\x90\x00").unwrap();
        drop(f);
        assert!(validate_downloaded_file(&path).is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_invalid_exe_is_deleted() {
        let dir = std::env::temp_dir().join("upmc_test_validate_bad_exe");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.exe");
        fs::write(&path, b"<html>404</html>").unwrap();
        assert!(validate_downloaded_file(&path).is_err());
        assert!(!path.exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn safe_zip_output_path_accepts_relative_path() {
        let dest = Path::new("C:/mc");
        let out = safe_zip_output_path(dest, "config/options.txt").unwrap();
        assert!(out.ends_with("config/options.txt"));
    }

    #[test]
    fn safe_zip_output_path_rejects_parent_dir() {
        let dest = Path::new("C:/mc");
        assert!(safe_zip_output_path(dest, "../blocked.txt").is_err());
    }

    #[test]
    fn safe_zip_output_path_rejects_root_dir() {
        let dest = Path::new("C:/mc");
        assert!(safe_zip_output_path(dest, "/blocked.txt").is_err());
    }

    #[test]
    fn validate_download_url_allows_trusted_suffix() {
        assert!(validate_download_url("https://github.com/a/b").is_ok());
        assert!(validate_download_url("https://raw.githubusercontent.com/a/b").is_ok());
    }

    #[test]
    fn validate_download_url_rejects_untrusted_host() {
        assert!(validate_download_url("https://example.invalid/file.zip").is_err());
    }

    #[test]
    fn validate_sha256_hex_checks_format() {
        assert!(validate_sha256_hex(&"a".repeat(64)).is_ok());
        assert!(validate_sha256_hex("abc").is_err());
    }
}
