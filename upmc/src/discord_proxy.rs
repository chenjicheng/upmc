// ============================================================
// discord_proxy.rs — Discord 代理设置模块
// ============================================================
// 三个职责：
//   1. setup()              — 首次配置/重新配置（拉订阅、下载 Xray、安装 DLL）
//   2. auto_start()         — 已配置过时自动启动 Xray（不重新订阅）
//   3. refresh_if_installed — 静默刷新 DLL
// ============================================================

use anyhow::{Context, Result};
use std::path::Path;

use crate::config;
use crate::update::Progress;
use crate::xray;

fn proxy_config() -> discord_voice_proxy::ProxyConfig {
    discord_voice_proxy::ProxyConfig {
        address: "127.0.0.1".to_string(),
        port: config::XRAY_SOCKS_PORT,
        login: None,
        password: None,
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
/// 拉取订阅、下载/更新 Xray、生成配置、启动 Xray、安装 DLL 到 Discord。
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
    discord_voice_proxy::installer::install_and_run(DWRITE_DLL, FORCE_PROXY_DLL, &proxy_config())?;

    on_progress(Progress::new(100, "Discord 代理已启用"));
    Ok(())
}

/// 已配置过时自动启动 Xray（不重新订阅，使用已有 config.json）。
/// 同时刷新 DLL。不重启 Discord。静默操作，不发送 progress。
pub fn auto_start(base_dir: &Path) -> Result<()> {
    let noop = |_: Progress| {};
    xray::download_or_update(base_dir, &noop)?;
    xray::start(base_dir)?;
    refresh_if_installed();
    Ok(())
}

/// 静默刷新已安装的代理 DLL（不启动 Xray，不重启 Discord）。
pub fn refresh_if_installed() {
    let Ok(is_proxy) = discord_voice_proxy::installer::is_installed() else {
        return;
    };
    if !is_proxy {
        return;
    }

    if let Err(e) =
        discord_voice_proxy::installer::ensure_installed(DWRITE_DLL, FORCE_PROXY_DLL, &proxy_config())
    {
        eprintln!("静默刷新 Discord 代理 DLL 失败: {e:#}");
    }
}
