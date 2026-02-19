// ============================================================
// version.rs — 版本检查模块
// ============================================================
// 负责：
//   1. 从远程 URL 拉取 server.json（包含最新 MC/Fabric 版本）
//   2. 读取本地 local.json（记录当前已安装的版本）
//   3. 对比两者，判断是否需要升级
// ============================================================

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::config;

/// 服务器端的版本信息（从远程 server.json 反序列化）
///
/// 示例 JSON：
/// ```json
/// {
///     "mc_version": "1.21.4",
///     "fabric_version": "0.16.9",
///     "version_tag": "fabric-loader-0.16.9-1.21.4",
///     "pack_url": "https://xxx.github.io/upmc-dist/pack.toml"
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerVersion {
    /// Minecraft 版本号，如 "1.21.4"
    pub mc_version: String,

    /// Fabric Loader 版本号，如 "0.16.9"
    pub fabric_version: String,

    /// 版本文件夹名称，如 "fabric-loader-0.16.9-1.21.4"
    /// 对应 .minecraft/versions/ 下的目录名
    pub version_tag: String,

    /// packwiz pack.toml 的远程 URL
    pub pack_url: String,

    /// 可选的下载 URL 配置（首次安装时自动下载组件）
    #[serde(default)]
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
}

/// 本地已安装的版本信息（保存在 local.json）
/// 结构与 ServerVersion 相同，方便直接序列化/反序列化。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalVersion {
    pub mc_version: String,
    pub fabric_version: String,
    pub version_tag: String,
}

/// 从远程 URL 拉取 server.json 并解析。
///
/// 如果网络不可用或超时，返回 Err，调用方应跳过更新。
pub fn fetch_remote_version() -> Result<ServerVersion> {
    // 使用 ureq 发送 GET 请求，设置超时
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(config::HTTP_TIMEOUT_SECS))
        .build();

    let response = agent
        .get(config::REMOTE_SERVER_JSON_URL)
        .call()
        .context("无法连接到更新服务器，请检查网络")?;

    let body = response
        .into_string()
        .context("读取服务器响应失败")?;

    let version: ServerVersion =
        serde_json::from_str(&body).context("解析 server.json 失败")?;

    Ok(version)
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
pub fn needs_version_upgrade(remote: &ServerVersion, local: &LocalVersion) -> bool {
    remote.mc_version != local.mc_version || remote.fabric_version != local.fabric_version
}
