// ============================================================
// config.rs — 配置常量 + Java 查找 + 更新通道
// ============================================================
// 集中管理所有可配置的路径和 URL。
// 修改这里的常量即可适配不同服务器。
// ============================================================

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

// ── 远程配置 ──

/// 远程 server.json 的 URL（GitHub Pages 托管，packwiz 仓库）
pub const REMOTE_SERVER_JSON_URL: &str =
    "https://update.mc.chenjicheng.cn/server.json";

/// 更新器版本信息 URL — 稳定通道（GitHub Pages 托管，upmc 仓库）
/// 返回 JSON: { "version": "x.y.z", "download_url": "..." }
pub const UPDATER_VERSION_URL: &str =
    "https://upmc.chenjicheng.cn/version.json";

/// 更新器版本信息 URL — 开发通道
/// 返回 JSON: { "version": "x.y.z", "download_url": "...", "build_id": "a1b2c3d" }
pub const UPDATER_DEV_VERSION_URL: &str =
    "https://upmc.chenjicheng.cn/dev/version.json";

// ── 更新通道 ──

/// 通道配置文件（相对于安装基准目录）
pub const CHANNEL_CONFIG_FILE: &str = "updater/channel.json";

/// 更新通道
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    /// 稳定通道：跟随正式 Release，使用语义化版本比较
    Stable,
    /// 开发通道：跟随 dev 分支最新构建，使用 build_id 比较
    Dev,
}

impl Default for UpdateChannel {
    fn default() -> Self {
        Self::Stable
    }
}

impl std::fmt::Display for UpdateChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stable => write!(f, "stable"),
            Self::Dev => write!(f, "dev"),
        }
    }
}

/// 通道配置文件内容
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelConfig {
    /// 当前选择的通道
    #[serde(default)]
    pub channel: UpdateChannel,

    /// dev 通道当前安装的构建 ID（7 位 commit SHA）
    /// 仅 dev 通道使用，用于判断是否需要更新
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_build_id: Option<String>,
}

/// 读取通道配置。文件不存在时返回默认值（Stable）。
pub fn read_channel_config(base_dir: &Path) -> ChannelConfig {
    let path = base_dir.join(CHANNEL_CONFIG_FILE);
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => ChannelConfig::default(),
    }
}

/// 保存通道配置到 channel.json。
pub fn save_channel_config(base_dir: &Path, config: &ChannelConfig) -> Result<()> {
    let path = base_dir.join(CHANNEL_CONFIG_FILE);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("创建 updater 目录失败")?;
    }
    let json = serde_json::to_string_pretty(config).context("序列化通道配置失败")?;
    fs::write(&path, json).context("写入 channel.json 失败")?;
    Ok(())
}

/// 根据通道返回对应的更新器版本信息 URL。
pub fn updater_version_url(channel: UpdateChannel) -> &'static str {
    match channel {
        UpdateChannel::Stable => UPDATER_VERSION_URL,
        UpdateChannel::Dev => UPDATER_DEV_VERSION_URL,
    }
}

// ── 本地路径（相对于安装基准目录） ──

pub const LOCAL_VERSION_FILE: &str = "updater/local.json";
pub const PACKWIZ_BOOTSTRAP_JAR: &str = "updater/packwiz-installer-bootstrap.jar";
pub const FABRIC_INSTALLER_JAR: &str = "updater/fabric-installer.jar";
pub const MINECRAFT_DIR: &str = ".minecraft";
pub const PCL2_EXE: &str = "Plain Craft Launcher 2.exe";
pub const PCL2_SETUP_INI_PATH: &str = "Setup.ini";

/// Java 下载页面 URL（当系统未安装 Java 时自动打开）
pub const JAVA_DOWNLOAD_URL: &str =
    "https://mirrors.tuna.tsinghua.edu.cn/Adoptium/21/jre/x64/windows";

/// Java 未找到时返回的错误类型，GUI 据此 downcast 识别并显示友好安装提示。
#[derive(Debug)]
pub struct JavaNotFound;

impl std::fmt::Display for JavaNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "系统中未检测到 Java 环境。\n\
             正在尝试打开 Java 下载页面，如未自动打开请手动访问：\n\
             {JAVA_DOWNLOAD_URL}"
        )
    }
}

impl std::error::Error for JavaNotFound {}

// ── 安装目录 ──

/// 安装子目录名称
/// 所有游戏组件（PCL2、.minecraft 等）存放在 文档/CJC整合包/ 下
pub const INSTALL_DIR_NAME: &str = "CJC整合包";

/// 获取安装基准目录：用户文档文件夹下的 INSTALL_DIR_NAME 子目录。
///
/// 例如：`C:\Users\<用户>\Documents\CJC整合包\`
pub fn get_install_dir() -> PathBuf {
    let doc_dir = dirs::document_dir()
        .unwrap_or_else(|| {
            // 极端情况下无法获取文档文件夹，回退到 exe 所在目录
            eprintln!("警告: 无法获取文档文件夹，回退到 exe 目录");
            std::env::current_exe()
                .expect("无法获取 exe 路径")
                .parent()
                .expect("无法获取 exe 所在目录")
                .to_path_buf()
        });
    doc_dir.join(INSTALL_DIR_NAME)
}

/// 获取旧版安装目录（exe 所在目录下的子目录），用于迁移检测。
pub fn get_legacy_install_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .expect("无法获取 exe 路径")
        .parent()
        .expect("无法获取 exe 所在目录")
        .to_path_buf();
    exe_dir.join(INSTALL_DIR_NAME)
}

// ── GUI ──

/// 生成窗口标题。
///
/// - Stable: `我的服务器 - 更新器 v0.3.6`
/// - Dev:    `我的服务器 - 更新器 dev-a1b2c3d`（7 位 commit SHA）
/// - Dev（无 build_id）: `我的服务器 - 更新器 dev`
pub fn window_title(channel: UpdateChannel, dev_build_id: Option<&str>) -> String {
    match channel {
        UpdateChannel::Stable => {
            format!("我的服务器 - 更新器 v{}", env!("CARGO_PKG_VERSION"))
        }
        UpdateChannel::Dev => {
            if let Some(id) = dev_build_id {
                let short = if id.len() >= 7 { &id[..7] } else { id };
                format!("我的服务器 - 更新器 dev-{short}")
            } else {
                "我的服务器 - 更新器 dev".to_string()
            }
        }
    }
}

// ── Windows 进程创建标志 ──

/// 创建子进程时不弹出控制台窗口
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ── 超时 ──

/// 小文件请求超时（server.json 等）
pub const HTTP_TIMEOUT_SECS: u64 = 30;
/// 大文件下载超时
pub const DOWNLOAD_TIMEOUT_SECS: u64 = 600;

// ── 重试 ──

/// 网络操作最大重试次数（含首次尝试）
pub const RETRY_MAX_ATTEMPTS: u32 = 3;
/// 首次重试前等待秒数（后续指数退避：3s → 6s）
pub const RETRY_BASE_DELAY_SECS: u64 = 3;

// ── PCL2 配置模板 ──

/// 首次安装时自动生成的 Setup.ini
///
/// 关键设置：
///   - VersionArgumentIndie=1: 不隔离，使用 .minecraft/ 作为游戏目录
///     （packwiz 把 mods/config 等安装到 .minecraft/，必须关闭隔离）
///   - HiddenPageDownload: 隐藏下载页，防止玩家误操作
///
/// PCL2 会自动检测同目录下的 .minecraft 文件夹，
/// 无需手动指定游戏目录。
pub const PCL2_SETUP_INI: &str = "\
; ===== 服务器专属启动器 =====\r\n\
Logo=我的服务器\r\n\
LogoSub=专属启动器\r\n\
; 不隔离版本，使用 .minecraft 作为游戏目录\r\n\
VersionArgumentIndie=1\r\n\
; 默认游戏窗口大小 720p\r\n\
LaunchArgumentWindowWidth=1280\r\n\
LaunchArgumentWindowHeight=720\r\n\
";

// ── Java 查找 ──

/// 自动查找 Java 可执行文件。
///
/// 搜索顺序：
///   1. JAVA_HOME 环境变量
///   2. 系统 PATH
///
/// 如果找不到 Java，会自动打开 Java 下载页面并返回错误。
pub fn find_java() -> Result<PathBuf> {
    // 1. JAVA_HOME
    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(&java_home).join("bin/java.exe");
        if p.exists() {
            return Ok(p);
        }
    }

    // 2. PATH（使用 where 命令查找）
    if let Ok(output) = Command::new("where").arg("java").creation_flags(CREATE_NO_WINDOW).output()
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(first_line) = stdout.lines().next() {
            let p = PathBuf::from(first_line.trim());
            if p.exists() {
                return Ok(p);
            }
        }
    }

    // 自动打开 Java 下载页面
    let _ = Command::new("cmd")
        .args(["/c", "start", "", JAVA_DOWNLOAD_URL])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();

    Err(anyhow::Error::new(JavaNotFound))
}
