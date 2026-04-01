// ============================================================
// discord_proxy.rs — Discord 代理设置模块
// ============================================================

use anyhow::{Context, Result};
use std::path::Path;

use crate::config;
use crate::update::Progress;
use crate::xray;

fn proxy_config(base_dir: &Path) -> discord_voice_proxy::ProxyConfig {
    let settings = config::load_user_settings(base_dir);
    discord_voice_proxy::ProxyConfig {
        address: "127.0.0.1".to_string(),
        port: config::XRAY_SOCKS_PORT,
        login: None,
        password: None,
        udp: settings.proxy_udp,
    }
}

const DWRITE_DLL: &[u8] = include_bytes!("../../target/release/dwrite.dll");
const FORCE_PROXY_DLL: &[u8] = include_bytes!("../../target/release/force_proxy.dll");

/// 检查是否已经配置过代理（xray config.json 存在）。
pub fn is_configured(base_dir: &Path) -> bool {
    base_dir
        .join(config::XRAY_DIR)
        .join("config.json")
        .exists()
}

/// 完整的首次配置/重新配置流程（用户点击按钮触发）。
pub fn setup(base_dir: &Path, on_progress: &dyn Fn(Progress)) -> Result<()> {
    xray::download_or_update(base_dir, on_progress)?;

    on_progress(Progress::new(38, "正在获取代理订阅..."));
    let configs = xray::fetch_subscription(config::SUBSCRIPTION_URL)?;
    let vless = configs
        .first()
        .context("没有可用的 REALITY 代理配置")?;
    on_progress(Progress::new(42, format!("使用代理节点: {}", vless.name)));

    on_progress(Progress::new(44, "正在配置 Xray..."));
    let xray_json = xray::generate_config(vless, config::XRAY_SOCKS_PORT);
    let xray_dir = base_dir.join(config::XRAY_DIR);
    std::fs::write(xray_dir.join("config.json"), &xray_json).context("写入 Xray 配置失败")?;

    on_progress(Progress::new(48, "正在启动 Xray..."));
    xray::start(base_dir)?;

    on_progress(Progress::new(60, "正在安装 Discord 代理..."));
    on_progress(Progress::new(80, "正在重启 Discord..."));
    discord_voice_proxy::installer::install_and_run(DWRITE_DLL, FORCE_PROXY_DLL, &proxy_config(base_dir))?;

    on_progress(Progress::new(100, "Discord 代理已启用"));
    Ok(())
}

/// 已配置过时自动启动 Xray + 安装 DLL。
/// 如果 Xray 启动失败，不安装 DLL（防止 Discord 卡死）。
pub fn auto_start(base_dir: &Path) -> Result<()> {
    let noop = |_: Progress| {};
    xray::download_or_update(base_dir, &noop)?;
    // Xray 必须成功启动，才安装 DLL
    xray::start(base_dir)?;
    install_dlls(base_dir);
    Ok(())
}

/// 停止代理：杀 Xray + 卸载 Discord DLL。
pub fn stop(base_dir: &Path) {
    xray::kill(base_dir);
    let _ = discord_voice_proxy::installer::uninstall();
}

/// 安装/刷新 DLL 到 Discord（仅写入缺失的文件）。
fn install_dlls(base_dir: &Path) {
    if let Err(e) =
        discord_voice_proxy::installer::ensure_installed(DWRITE_DLL, FORCE_PROXY_DLL, &proxy_config(base_dir))
    {
        eprintln!("安装 Discord 代理 DLL 失败: {e:#}");
    }
}
