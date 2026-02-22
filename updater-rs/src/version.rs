// ============================================================
// version.rs — 版本检查模块
// ============================================================
// 负责：
//   1. 从远程 URL 拉取 server.json（包含 pack_url 和下载配置）
//   2. 从 pack.toml 解析 MC/Fabric 版本（单一数据源）
//   3. 读取本地 local.json（记录当前已安装的版本）
//   4. 对比两者，判断是否需要升级
// ============================================================

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::config;
use crate::retry;

/// 服务器端配置（从远程 server.json 反序列化）
///
/// 只包含 pack_url 和 downloads，版本信息从 pack.toml 读取。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// packwiz pack.toml 的远程 URL
    pub pack_url: String,

    /// 可选的下载 URL 配置（首次安装时自动下载组件）
    #[serde(default)]
    pub downloads: Downloads,
}

/// 从 pack.toml 解析出的版本信息 + server.json 的配置合并后的完整远程状态
#[derive(Debug, Clone)]
pub struct RemoteVersion {
    /// Minecraft 版本号，如 "1.21.11"
    pub mc_version: String,

    /// Fabric Loader 版本号，如 "0.18.4"
    pub fabric_version: String,

    /// 版本文件夹名称，如 "fabric-loader-0.18.4-1.21.11"
    pub version_tag: String,

    /// packwiz pack.toml 的远程 URL
    pub pack_url: String,

    /// 下载配置
    pub downloads: Downloads,
}

/// 首次安装所需的下载 URL 集合。
///
/// jre_url / packwiz_bootstrap_url 有内置默认值，
/// pcl2_url / fabric_installer_url 需管理员配置。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Downloads {
    /// Java 运行时下载地址（.zip）
    #[serde(default)]
    pub jre_url: Option<String>,

    /// PCL2 启动器下载地址（管理员托管在 GitHub Releases 等）
    #[serde(default)]
    pub pcl2_url: Option<String>,

    /// packwiz-installer-bootstrap.jar 下载地址
    #[serde(default)]
    pub packwiz_bootstrap_url: Option<String>,

    /// Fabric Installer jar 下载地址
    #[serde(default)]
    pub fabric_installer_url: Option<String>,

    /// 首次安装设置包下载地址（.zip）
    /// 解压到 .minecraft/ 目录，包含默认游戏设置和模组配置。
    /// 仅首次安装时下载，不会覆盖已有的个人设置。
    ///
    /// ZIP 结构示例：
    ///   options.txt          ← 视频/按键/语言
    ///   servers.dat          ← 预填服务器地址
    ///   config/              ← 模组默认配置
    ///   shaderpacks/         ← 光影预设
    #[serde(default)]
    pub settings_url: Option<String>,

    // 注意：updater_url 和 updater_version 已迁移到独立的 version.json
    // (upmc.chenjicheng.cn/version.json)，由 selfupdate 模块独立获取。
    // 这两个字段保留以兼容旧版 server.json 反序列化，但不再使用。
    #[serde(default)]
    _updater_url: Option<String>,
    #[serde(default)]
    _updater_version: Option<String>,
}

/// 本地已安装的版本信息（保存在 local.json）
/// 结构与 ServerVersion 相同，方便直接序列化/反序列化。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalVersion {
    pub mc_version: String,
    pub fabric_version: String,
    pub version_tag: String,
}

/// 从远程拉取 server.json 和 pack.toml，合并为完整的远程版本信息。
///
/// 流程：
///   1. GET server.json → 获取 pack_url 和 downloads
///   2. GET pack.toml   → 解析 minecraft 和 fabric 版本
///   3. 合并为 RemoteVersion
pub fn fetch_remote_version() -> Result<RemoteVersion> {
    retry::with_retry(
        config::RETRY_MAX_ATTEMPTS,
        config::RETRY_BASE_DELAY_SECS,
        "获取远程版本信息",
        || fetch_remote_version_inner(),
    )
}

/// fetch_remote_version 的内部实现（单次尝试）。
fn fetch_remote_version_inner() -> Result<RemoteVersion> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(config::HTTP_TIMEOUT_SECS)))
        .build()
        .into();

    // 1. 拉取 server.json
    let body = agent
        .get(config::REMOTE_SERVER_JSON_URL)
        .call()
        .context("无法连接到更新服务器，请检查网络")?
        .body_mut()
        .read_to_string()
        .context("读取服务器响应失败")?;

    let server_config: ServerConfig =
        serde_json::from_str(&body).context("解析 server.json 失败")?;

    // 2. 拉取 pack.toml 并解析版本
    let pack_toml = agent
        .get(&server_config.pack_url)
        .call()
        .context("无法获取 pack.toml，请检查网络")?
        .body_mut()
        .read_to_string()
        .context("读取 pack.toml 失败")?;

    let (mc_version, fabric_version) = parse_pack_toml_versions(&pack_toml)
        .context("从 pack.toml 解析版本信息失败")?;

    // 3. 合并
    let version_tag = format!("fabric-loader-{fabric_version}-{mc_version}");

    Ok(RemoteVersion {
        mc_version,
        fabric_version,
        version_tag,
        pack_url: server_config.pack_url,
        downloads: server_config.downloads,
    })
}

/// 从 pack.toml 文本中解析 minecraft 和 fabric 版本。
///
/// pack.toml 格式示例：
/// ```toml
/// [versions]
/// fabric = "0.18.4"
/// minecraft = "1.21.11"
/// ```
///
/// 使用简单字符串解析，不需要完整的 TOML 解析器。
fn parse_pack_toml_versions(toml_text: &str) -> Result<(String, String)> {
    let mut mc_version: Option<String> = None;
    let mut fabric_version: Option<String> = None;
    let mut in_versions_section = false;

    for line in toml_text.lines() {
        let trimmed = line.trim();

        // 检测 [versions] 段
        if trimmed == "[versions]" {
            in_versions_section = true;
            continue;
        }

        // 遇到新的段落 [xxx]，退出 versions 段
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

/// 从 TOML 行中提取 `key = "value"` 形式的值
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

    // 处理带引号的字符串值：提取第一对引号之间的内容
    // 这样可以正确忽略行内注释，如 key = "value" # comment
    if let Some(stripped) = value_part.strip_prefix('"') {
        // 找到闭合引号
        let end = stripped.find('"').unwrap_or(stripped.len());
        return Some(stripped[..end].to_string());
    }
    if let Some(stripped) = value_part.strip_prefix('\'') {
        let end = stripped.find('\'').unwrap_or(stripped.len());
        return Some(stripped[..end].to_string());
    }

    // 无引号的值：截断行内注释
    let value = if let Some(hash_pos) = value_part.find('#') {
        value_part[..hash_pos].trim()
    } else {
        value_part
    };
    Some(value.to_string())
}

/// 读取本地 local.json。
///
/// 如果文件不存在（首次运行），返回一个空的 LocalVersion，
/// 这样对比时一定会触发完整安装。
pub fn read_local_version(base_dir: &Path) -> LocalVersion {
    let path = base_dir.join(config::LOCAL_VERSION_FILE);

    // 尝试读取并解析，失败则返回默认值（全部字段为空字符串）
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => LocalVersion::default(),
    }
}

/// 将当前版本信息写入 local.json，供下次启动时对比。
pub fn save_local_version(base_dir: &Path, version: &LocalVersion) -> Result<()> {
    let path = base_dir.join(config::LOCAL_VERSION_FILE);

    // 确保 updater/ 目录存在
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("创建 updater 目录失败")?;
    }

    let json = serde_json::to_string_pretty(version).context("序列化版本信息失败")?;
    fs::write(&path, json).context("写入 local.json 失败")?;

    Ok(())
}

/// 判断是否需要升级 Minecraft / Fabric 版本。
///
/// 只要 mc_version 或 fabric_version 任意一个不同，就需要升级。
pub fn needs_version_upgrade(remote: &RemoteVersion, local: &LocalVersion) -> bool {
    remote.mc_version != local.mc_version || remote.fabric_version != local.fabric_version
}
