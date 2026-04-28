// ============================================================
// version.rs — 版本检查模块
// ============================================================

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::config;
use crate::retry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// packwiz pack.toml 的远程 URL
    pub pack_url: String,

    /// 可选的下载 URL 配置（首次安装时自动下载组件）
    #[serde(default)]
    pub downloads: Downloads,
}

#[derive(Debug, Clone)]
pub struct RemoteVersion {
    pub mc_version: String,
    pub fabric_version: String,
    pub version_tag: String,
    pub pack_url: String,
    pub downloads: Downloads,
    pub pack_toml_raw: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Downloads {
    /// Java 运行时下载地址（.zip）
    #[serde(default)]
    pub jre_url: Option<String>,

    /// Java 运行时 ZIP 的 SHA256。当前版本暂未使用，保留兼容后续启用。
    #[serde(default)]
    pub jre_sha256: Option<String>,

    /// PCL2 启动器下载地址（管理员托管在 GitHub Releases 等）
    #[serde(default)]
    pub pcl2_url: Option<String>,

    /// PCL2 启动器 EXE 的 SHA256
    #[serde(default)]
    pub pcl2_sha256: Option<String>,

    /// packwiz-installer-bootstrap.jar 下载地址
    #[serde(default)]
    pub packwiz_bootstrap_url: Option<String>,

    /// packwiz-installer-bootstrap.jar 的 SHA256
    #[serde(default)]
    pub packwiz_bootstrap_sha256: Option<String>,

    /// Fabric Installer jar 下载地址
    #[serde(default)]
    pub fabric_installer_url: Option<String>,

    /// Fabric Installer jar 的 SHA256
    #[serde(default)]
    pub fabric_installer_sha256: Option<String>,

    /// 首次安装设置包下载地址（.zip）
    #[serde(default)]
    pub settings_url: Option<String>,

    /// 首次安装设置包 ZIP 的 SHA256。settings_url 存在时必须提供。
    #[serde(default)]
    pub settings_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalVersion {
    pub mc_version: String,
    pub fabric_version: String,
    pub version_tag: String,
}

pub fn fetch_remote_version() -> Result<RemoteVersion> {
    retry::with_retry(
        config::RETRY_MAX_ATTEMPTS,
        config::RETRY_BASE_DELAY_SECS,
        "获取远程版本信息",
        || fetch_remote_version_inner(),
    )
}

fn fetch_remote_version_inner() -> Result<RemoteVersion> {
    let agent = config::http_agent();

    let body = agent
        .get(config::REMOTE_SERVER_JSON_URL)
        .call()
        .context("无法连接到更新服务器，请检查网络")?
        .body_mut()
        .read_to_string()
        .context("读取服务器响应失败")?;

    let server_config: ServerConfig =
        serde_json::from_str(&body).context("解析 server.json 失败")?;

    if !server_config.pack_url.starts_with("https://") {
        anyhow::bail!("pack_url 必须使用 HTTPS 协议: {}", server_config.pack_url);
    }

    let pack_toml = agent
        .get(&server_config.pack_url)
        .call()
        .context("无法获取 pack.toml，请检查网络")?
        .body_mut()
        .read_to_string()
        .context("读取 pack.toml 失败")?;

    let (mc_version, fabric_version) = parse_pack_toml_versions(&pack_toml)
        .context("从 pack.toml 解析版本信息失败")?;

    validate_version_string(&mc_version).context("minecraft 版本号包含非法字符")?;
    validate_version_string(&fabric_version).context("fabric 版本号包含非法字符")?;

    let version_tag = format!("fabric-loader-{fabric_version}-{mc_version}");

    Ok(RemoteVersion {
        mc_version,
        fabric_version,
        version_tag,
        pack_url: server_config.pack_url,
        downloads: server_config.downloads,
        pack_toml_raw: pack_toml,
    })
}

fn validate_version_string(version: &str) -> Result<()> {
    if version.is_empty()
        || version.contains('/')
        || version.contains('\\')
        || version.contains("..")
    {
        anyhow::bail!("版本号 \"{version}\" 包含非法字符");
    }
    Ok(())
}

fn parse_pack_toml_versions(toml_text: &str) -> Result<(String, String)> {
    let mut mc_version: Option<String> = None;
    let mut fabric_version: Option<String> = None;
    let mut in_versions_section = false;

    for line in toml_text.lines() {
        let trimmed = line.trim();

        if trimmed == "[versions]" {
            in_versions_section = true;
            continue;
        }

        if trimmed.starts_with('[') && in_versions_section {
            break;
        }

        if in_versions_section {
            if let Some(value) = extract_toml_value(trimmed, "minecraft") {
                mc_version = Some(value);
            }
            if let Some(value) = extract_toml_value(trimmed, "fabric") {
                fabric_version = Some(value);
            }
        }
    }

    let mc = mc_version.context("pack.toml 中找不到 minecraft 版本")?;
    let fabric = fabric_version.context("pack.toml 中找不到 fabric 版本")?;

    Ok((mc, fabric))
}

fn extract_toml_value(line: &str, key: &str) -> Option<String> {
    let line = line.trim();
    if !line.starts_with(key) {
        return None;
    }

    let rest = line[key.len()..].trim();
    if !rest.starts_with('=') {
        return None;
    }

    let value_part = rest[1..].trim();

    if let Some(stripped) = value_part.strip_prefix('"') {
        let end = stripped.find('"').unwrap_or(stripped.len());
        return Some(stripped[..end].to_string());
    }

    if let Some(stripped) = value_part.strip_prefix('\'') {
        let end = stripped.find('\'').unwrap_or(stripped.len());
        return Some(stripped[..end].to_string());
    }

    let value = if let Some(hash_pos) = value_part.find('#') {
        value_part[..hash_pos].trim()
    } else {
        value_part
    };
    Some(value.to_string())
}

pub fn read_local_version(base_dir: &Path) -> LocalVersion {
    let path = base_dir.join(config::LOCAL_VERSION_FILE);
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => LocalVersion::default(),
    }
}

pub fn save_local_version(base_dir: &Path, version: &LocalVersion) -> Result<()> {
    let path = base_dir.join(config::LOCAL_VERSION_FILE);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("创建 updater 目录失败")?;
    }

    let json = serde_json::to_string_pretty(version).context("序列化版本信息失败")?;
    fs::write(&path, json).context("写入 local.json 失败")?;
    Ok(())
}

pub fn needs_version_upgrade(remote: &RemoteVersion, local: &LocalVersion) -> bool {
    remote.mc_version != local.mc_version || remote.fabric_version != local.fabric_version
}

pub fn is_pack_changed(base_dir: &Path, remote_pack_toml: &str) -> bool {
    let cache_path = base_dir.join(config::PACK_TOML_CACHE_FILE);
    match fs::read_to_string(&cache_path) {
        Ok(cached) => cached != remote_pack_toml,
        Err(_) => true,
    }
}

pub fn save_pack_cache(base_dir: &Path, pack_toml: &str) -> Result<()> {
    let cache_path = base_dir.join(config::PACK_TOML_CACHE_FILE);
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent).context("创建 updater 目录失败")?;
    }
    fs::write(&cache_path, pack_toml).context("写入 pack.toml 缓存失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_standard_pack_toml() {
        let toml = r#"
[pack]
name = "test-pack"
version = "1.0.0"

[versions]
fabric = "0.18.4"
minecraft = "1.21.11"

[index]
file = "index.toml"
"#;
        let (mc, fabric) = parse_pack_toml_versions(toml).unwrap();
        assert_eq!(mc, "1.21.11");
        assert_eq!(fabric, "0.18.4");
    }

    #[test]
    fn parse_versions_with_inline_comments() {
        let toml = r#"
[versions]
minecraft = "1.20.4" # latest stable
fabric = "0.15.0" # required
"#;
        let (mc, fabric) = parse_pack_toml_versions(toml).unwrap();
        assert_eq!(mc, "1.20.4");
        assert_eq!(fabric, "0.15.0");
    }

    #[test]
    fn parse_missing_versions() {
        assert!(parse_pack_toml_versions("[versions]\nfabric = \"0.18.4\"").is_err());
        assert!(parse_pack_toml_versions("[versions]\nminecraft = \"1.21.11\"").is_err());
        assert!(parse_pack_toml_versions("[pack]\nname = \"test\"").is_err());
    }

    #[test]
    fn extract_no_false_prefix_match() {
        assert_eq!(
            extract_toml_value("fabric_loader = \"0.18\"", "fabric"),
            None
        );
    }

    #[test]
    fn validate_version_string_checks_unsafe_chars() {
        assert!(validate_version_string("1.21.11").is_ok());
        assert!(validate_version_string("0.18.4").is_ok());
        assert!(validate_version_string("").is_err());
        assert!(validate_version_string("1.21/blocked").is_err());
        assert!(validate_version_string("1.21\\blocked").is_err());
        assert!(validate_version_string("1.21..11").is_err());
    }
}
