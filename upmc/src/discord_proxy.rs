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

/// 检查是否已经配置过代理（xray config.json 存在，且本机安装了 Discord）。
pub fn is_configured(base_dir: &Path) -> bool {
    base_dir.join(config::XRAY_DIR).join("config.json").exists() && is_discord_installed()
}

/// 检查是否应在软件启动时恢复代理。
///
/// 配置文件存在只代表代理曾经配置过；用户手动停止后，
/// `proxy_enabled` 会阻止下次启动自动恢复。
pub fn should_auto_start(base_dir: &Path) -> bool {
    is_configured(base_dir) && config::load_user_settings(base_dir).proxy_enabled
}

/// 检查本机是否安装了 Discord。
pub fn is_discord_installed() -> bool {
    discord_voice_proxy::discord::is_installed()
}

/// 完整的首次配置/重新配置流程（用户点击按钮触发）。
pub fn setup(base_dir: &Path, on_progress: &dyn Fn(Progress)) -> Result<()> {
    ensure_discord_installed()?;
    xray::download_or_update(base_dir, on_progress)?;

    on_progress(Progress::new(38, "正在获取代理订阅..."));
    let configs = xray::fetch_subscription(config::SUBSCRIPTION_URL)?;
    let vless = configs.first().context("没有可用的 REALITY 代理配置")?;
    on_progress(Progress::new(42, format!("使用代理节点: {}", vless.name)));

    on_progress(Progress::new(44, "正在配置 Xray..."));
    let xray_json = xray::generate_config(vless, config::XRAY_SOCKS_PORT);
    let xray_dir = base_dir.join(config::XRAY_DIR);
    std::fs::write(xray_dir.join("config.json"), &xray_json).context("写入 Xray 配置失败")?;

    on_progress(Progress::new(48, "正在启动 Xray..."));
    xray::start(base_dir)?;

    on_progress(Progress::new(60, "正在安装 Discord 代理..."));
    on_progress(Progress::new(80, "正在重启 Discord..."));
    if let Err(e) = discord_voice_proxy::installer::install_and_run(
        DWRITE_DLL,
        FORCE_PROXY_DLL,
        &proxy_config(base_dir),
    ) {
        xray::kill(base_dir);
        return Err(e).context("安装 Discord 代理失败");
    }

    if let Err(e) = set_proxy_enabled(base_dir, true) {
        xray::kill(base_dir);
        let _ = discord_voice_proxy::installer::uninstall();
        return Err(e);
    }

    on_progress(Progress::new(100, "Discord 代理已启用"));
    Ok(())
}

/// 已配置过时自动启动 Xray + 安装 DLL。
/// 如果 Xray 启动失败，不安装 DLL（防止 Discord 卡死）。
pub fn auto_start(base_dir: &Path) -> Result<()> {
    ensure_discord_installed()?;
    let noop = |_: Progress| {};
    xray::download_or_update(base_dir, &noop)?;
    // Xray 必须成功启动，才安装 DLL
    xray::start(base_dir)?;
    if let Err(e) = install_dlls(base_dir) {
        xray::kill(base_dir);
        return Err(e);
    }
    Ok(())
}

/// 停止代理：记住用户选择 + 杀 Xray + 卸载 Discord DLL。
pub fn stop(base_dir: &Path) -> Result<()> {
    // 先持久化用户意图，即使后续清理失败，下次启动也不会重新安装代理。
    let persist_result = set_proxy_enabled(base_dir, false);
    xray::kill(base_dir);
    let uninstall_result =
        discord_voice_proxy::installer::uninstall().context("卸载 Discord 代理 DLL 失败");

    persist_result?;
    uninstall_result?;
    Ok(())
}

/// 安装/刷新 DLL 到 Discord（仅写入缺失的文件）。
fn install_dlls(base_dir: &Path) -> Result<()> {
    discord_voice_proxy::installer::ensure_installed(
        DWRITE_DLL,
        FORCE_PROXY_DLL,
        &proxy_config(base_dir),
    )
    .context("安装 Discord 代理 DLL 失败")
}

fn ensure_discord_installed() -> Result<()> {
    if is_discord_installed() {
        Ok(())
    } else {
        anyhow::bail!("未检测到 Discord，请先安装 Discord 后再启用代理");
    }
}

fn set_proxy_enabled(base_dir: &Path, enabled: bool) -> Result<()> {
    let mut settings = config::load_user_settings(base_dir);
    settings.proxy_enabled = enabled;
    config::save_user_settings(base_dir, &settings).with_context(|| {
        if enabled {
            "保存代理启用状态失败"
        } else {
            "保存代理停止状态失败，重新打开软件时可能再次启动代理"
        }
    })
}
