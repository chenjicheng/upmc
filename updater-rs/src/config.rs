// ============================================================
// config.rs — 配置常量 + Java 查找
// ============================================================
// 集中管理所有可配置的路径和 URL。
// 修改这里的常量即可适配不同服务器。
// ============================================================

use anyhow::{bail, Result};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

// ── 远程配置 ──

/// 远程 server.json 的 URL（GitHub Pages 托管）
pub const REMOTE_SERVER_JSON_URL: &str =
    "https://update.mc.chenjicheng.cn/server.json";

// ── 本地路径（相对于 exe 所在目录） ──

pub const LOCAL_VERSION_FILE: &str = "updater/local.json";
pub const PACKWIZ_BOOTSTRAP_JAR: &str = "updater/packwiz-installer-bootstrap.jar";
pub const FABRIC_INSTALLER_JAR: &str = "updater/fabric-installer.jar";
pub const MINECRAFT_DIR: &str = ".minecraft";
pub const PCL2_EXE: &str = "Plain Craft Launcher 2.exe";
pub const PCL2_SETUP_INI_PATH: &str = "Setup.ini";
pub const LOCAL_JRE_JAVA: &str = "jre/bin/java.exe";

// ── 安装目录 ──

/// 组件安装子目录（相对于 exe 所在目录）
/// exe 本身在外层，所有下载内容（PCL2、JRE、.minecraft 等）在此子目录下
pub const INSTALL_DIR: &str = "CJC整合包";

// ── GUI ──

pub fn window_title() -> String {
    format!("我的服务器 - 更新器 v{}", env!("CARGO_PKG_VERSION"))
}

// ── Windows 进程创建标志 ──

/// 创建子进程时不弹出控制台窗口
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ── 超时 ──

/// 小文件请求超时（server.json 等）
pub const HTTP_TIMEOUT_SECS: u64 = 30;
/// 大文件下载超时（JRE ~50MB）
pub const DOWNLOAD_TIMEOUT_SECS: u64 = 600;

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
; 隐藏下载页面\r\n\
HiddenPageDownload=True\r\n\
; 默认游戏窗口大小 720p\r\n\
LaunchArgumentWindowWidth=1280\r\n\
LaunchArgumentWindowHeight=720\r\n\
";

// ── Java 查找 ──

/// 自动查找 Java 可执行文件。
///
/// 搜索顺序：
///   1. 本地下载的 JRE (jre/bin/java.exe)
///   2. JAVA_HOME 环境变量
///   3. 系统 PATH
pub fn find_java(base_dir: &Path) -> Result<PathBuf> {
    // 1. 本地 JRE（bootstrap 阶段下载的）
    let local = base_dir.join(LOCAL_JRE_JAVA);
    if local.exists() {
        return Ok(local);
    }

    // 2. JAVA_HOME
    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(&java_home).join("bin/java.exe");
        if p.exists() {
            return Ok(p);
        }
    }

    // 3. PATH（使用 where 命令查找）
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

    bail!(
        "找不到 Java。\n预期位置: {}\n请确保首次安装已完成。",
        local.display()
    )
}
