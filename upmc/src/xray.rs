// ============================================================
// xray.rs — Xray 代理核心管理模块
// ============================================================
// 职责：
//   1. 从 GitHub (通过镜像加速) 下载/更新 Xray-core
//   2. 从订阅 URL 获取 VLESS REALITY 代理配置
//   3. 生成 Xray config.json
//   4. 启动/停止 Xray 进程（本地 SOCKS5 代理）
// ============================================================

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::time::Duration;

use crate::bootstrap;
use crate::config;
use crate::retry;
use crate::update::Progress;

// ── GitHub Release API ─────────────────────────────────────

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

// ── VLESS 配置 ─────────────────────────────────────────────

/// 从订阅 URL 解析出的 VLESS REALITY 代理配置
pub struct VlessConfig {
    pub uuid: String,
    pub address: String,
    pub port: u16,
    pub flow: Option<String>,
    pub encryption: String,
    pub network: String,
    pub security: String,
    pub sni: String,
    pub public_key: String,
    pub short_id: String,
    pub spider_x: String,
    pub fingerprint: String,
    pub name: String,
}

// ── 公开 API ───────────────────────────────────────────────

/// 下载或更新 Xray。跳过已是最新版的情况。
pub fn download_or_update(base_dir: &Path, on_progress: &dyn Fn(Progress)) -> Result<()> {
    let xray_dir = base_dir.join(config::XRAY_DIR);
    std::fs::create_dir_all(&xray_dir).context("创建 Xray 目录失败")?;

    let xray_exe = xray_dir.join("xray.exe");
    let version_file = xray_dir.join("version.txt");
    let local_ver = std::fs::read_to_string(&version_file).unwrap_or_default();

    // 查询最新 Release
    on_progress(Progress::new(5, "检查 Xray 最新版本..."));
    let release = fetch_latest_release()?;

    if local_ver.trim() == release.tag_name && xray_exe.exists() {
        on_progress(Progress::new(10, "Xray 已是最新版本"));
        return Ok(());
    }

    // 找到对应平台的 asset
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == config::XRAY_ASSET_NAME)
        .context(format!(
            "Xray Release 中未找到 {} 资产",
            config::XRAY_ASSET_NAME
        ))?;

    // 通过 GitHub 镜像下载
    let download_url = format!("{}{}", config::GITHUB_PROXY, asset.browser_download_url);
    on_progress(Progress::new(10, format!("正在下载 Xray {}...", release.tag_name)));

    let zip_path = xray_dir.join("xray-download.zip");
    bootstrap::download_file(&download_url, &zip_path, on_progress, 10, 28)?;

    // 解压
    on_progress(Progress::new(30, "正在解压 Xray..."));
    bootstrap::extract_zip(&zip_path, &xray_dir)?;
    std::fs::remove_file(&zip_path).ok();

    // 记录版本
    std::fs::write(&version_file, &release.tag_name).context("保存 Xray 版本失败")?;

    on_progress(Progress::new(35, format!("Xray {} 就绪", release.tag_name)));
    Ok(())
}

/// 从订阅 URL 拉取代理列表，筛选 REALITY 协议。
pub fn fetch_subscription(url: &str) -> Result<Vec<VlessConfig>> {
    if url.is_empty() {
        bail!(
            "未配置代理订阅地址。\n\
             请在构建时通过环境变量 UPMC_SUB_URL 注入"
        );
    }

    let url_owned = url.to_string();
    let agent = config::http_agent();
    let text = retry::with_retry(
        config::RETRY_MAX_ATTEMPTS,
        config::RETRY_BASE_DELAY_SECS,
        "获取代理订阅",
        || {
            let body = agent
                .get(&url_owned)
                .call()
                .with_context(|| format!("获取订阅失败: {url_owned}"))?;
            body.into_body()
                .read_to_string()
                .context("读取订阅内容失败")
        },
    )?;

    // base64 解码
    let decoded = base64_decode(text.trim()).context("订阅 base64 解码失败")?;
    let content = String::from_utf8(decoded).context("订阅内容非有效 UTF-8")?;

    // 解析 VLESS URL，只保留 REALITY
    let mut configs = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("vless://") {
            if let Ok(cfg) = parse_vless(rest) {
                if cfg.security == "reality" {
                    configs.push(cfg);
                }
            }
        }
    }

    if configs.is_empty() {
        bail!("订阅中未找到 REALITY 代理配置");
    }

    Ok(configs)
}

/// 根据 VLESS 配置生成 Xray 的 config.json 内容。
/// 使用 serde_json 构建，防止订阅数据中的特殊字符导致 JSON 注入。
pub fn generate_config(vless: &VlessConfig, socks_port: u16) -> String {
    let mut user = serde_json::json!({
        "id": vless.uuid,
        "encryption": vless.encryption
    });
    if let Some(ref flow) = vless.flow {
        user["flow"] = serde_json::Value::String(flow.clone());
    }

    let config = serde_json::json!({
        "log": { "loglevel": "warning" },
        "inbounds": [{
            "port": socks_port,
            "listen": "127.0.0.1",
            "protocol": "socks",
            "settings": { "udp": true }
        }],
        "outbounds": [{
            "protocol": "vless",
            "settings": {
                "vnext": [{
                    "address": vless.address,
                    "port": vless.port,
                    "users": [user]
                }]
            },
            "streamSettings": {
                "network": vless.network,
                "security": "reality",
                "realitySettings": {
                    "fingerprint": vless.fingerprint,
                    "serverName": vless.sni,
                    "publicKey": vless.public_key,
                    "shortId": vless.short_id,
                    "spiderX": vless.spider_x
                }
            }
        }]
    });

    serde_json::to_string_pretty(&config).unwrap()
}

/// 写入配置并启动 Xray 进程（先结束旧进程）。
pub fn start(base_dir: &Path) -> Result<()> {
    kill(base_dir);

    let xray_dir = base_dir.join(config::XRAY_DIR);
    let xray_exe = xray_dir.join("xray.exe");

    if !xray_exe.exists() {
        bail!("xray.exe 不存在: {}", xray_exe.display());
    }

    let child = std::process::Command::new(&xray_exe)
        .arg("run")
        .arg("-config")
        .arg(xray_dir.join("config.json"))
        .current_dir(&xray_dir)
        .creation_flags(config::CREATE_NO_WINDOW)
        .spawn()
        .context("启动 Xray 失败")?;

    let _ = std::fs::write(xray_dir.join("xray.pid"), child.id().to_string());

    // 等待 SOCKS5 端口就绪并验证连通性
    wait_for_socks5(config::XRAY_SOCKS_PORT)?;
    Ok(())
}

/// 终止本程序启动的 Xray 进程。
pub fn kill(base_dir: &Path) {
    let pid_path = base_dir.join(config::XRAY_DIR).join("xray.pid");
    let mut killed = false;
    if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            let _ = std::process::Command::new("taskkill")
                .args(["/f", "/pid", &pid.to_string()])
                .creation_flags(config::CREATE_NO_WINDOW)
                .output();
            killed = true;
        }
        let _ = std::fs::remove_file(&pid_path);
    }
    if killed {
        std::thread::sleep(Duration::from_millis(500));
    }
}

// ── 内部实现 ───────────────────────────────────────────────

/// 验证 Xray 代理完整链路：本地 SOCKS5 → 远程服务器 → discord.com:443。
/// 最多等 15 秒（端口就绪 + 远程连接）。
fn wait_for_socks5(port: u16) -> Result<()> {
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);

    while std::time::Instant::now() < deadline {
        if let Ok(()) = test_socks5_connect(&addr, "discord.com", 443) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    bail!(
        "Xray 代理连通性检查失败（无法通过 127.0.0.1:{port} 连接到 discord.com）\n\
         可能原因：订阅节点不可用、Xray 配置错误、或网络问题"
    );
}

/// 通过 SOCKS5 代理尝试 CONNECT 到目标主机，验证完整链路。
fn test_socks5_connect(
    proxy: &std::net::SocketAddr,
    target_host: &str,
    target_port: u16,
) -> Result<()> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let mut stream = TcpStream::connect_timeout(proxy, Duration::from_secs(2))
        .context("无法连接到本地 SOCKS5 端口")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    // SOCKS5 握手
    stream.write_all(&[0x05, 0x01, 0x00])?;
    let mut resp = [0u8; 2];
    stream.read_exact(&mut resp)?;
    if resp[0] != 0x05 || resp[1] != 0x00 {
        bail!("SOCKS5 握手失败");
    }

    // SOCKS5 CONNECT: VER=5 CMD=1(CONNECT) RSV=0 ATYP=3(域名)
    let host_bytes = target_host.as_bytes();
    let mut req = Vec::with_capacity(7 + host_bytes.len());
    req.extend_from_slice(&[0x05, 0x01, 0x00, 0x03]);
    req.push(host_bytes.len() as u8);
    req.extend_from_slice(host_bytes);
    req.extend_from_slice(&target_port.to_be_bytes());
    stream.write_all(&req)?;

    // 读取回复（至少 10 字节: VER REP RSV ATYP ADDR PORT）
    let mut reply = [0u8; 10];
    stream.read_exact(&mut reply)?;
    if reply[0] != 0x05 || reply[1] != 0x00 {
        bail!("SOCKS5 CONNECT 到 {target_host}:{target_port} 失败 (REP={})", reply[1]);
    }

    Ok(())
}

fn fetch_latest_release() -> Result<GithubRelease> {
    let agent = config::http_agent();
    retry::with_retry(
        config::RETRY_MAX_ATTEMPTS,
        config::RETRY_BASE_DELAY_SECS,
        "获取 Xray 最新版本",
        || {
            let url = format!(
                "https://api.github.com/repos/{}/releases/latest",
                config::XRAY_GITHUB_REPO
            );

            let body = agent
                .get(&url)
                .header("User-Agent", "upmc")
                .call()
                .context("无法连接 GitHub API")?;

            let text = body.into_body().read_to_string().context("读取响应失败")?;
            serde_json::from_str(&text).context("解析 GitHub Release JSON 失败")
        },
    )
}

// ── VLESS URL 解析 ─────────────────────────────────────────

/// 解析 `uuid@host:port?params#name` (已去掉 vless:// 前缀)
fn parse_vless(input: &str) -> Result<VlessConfig> {
    // 片段（#名称）
    let (input, name) = match input.split_once('#') {
        Some((a, b)) => (a, percent_decode(b)),
        None => (input, String::new()),
    };

    // 查询参数
    let (authority, query) = input.split_once('?').unwrap_or((input, ""));
    let mut params = parse_query(query);

    // uuid@host:port
    let (uuid, host_port) = authority.split_once('@').context("VLESS URL 缺少 @")?;
    // 支持 IPv6: [::1]:443
    let (host, port_str) = if host_port.starts_with('[') {
        // IPv6
        let end = host_port.find(']').context("IPv6 地址缺少 ]")?;
        let host = &host_port[1..end];
        let port = host_port[end + 1..]
            .strip_prefix(':')
            .context("IPv6 地址后缺少端口")?;
        (host, port)
    } else {
        host_port.rsplit_once(':').context("VLESS URL 缺少端口")?
    };
    let port: u16 = port_str.parse().context("端口号无效")?;

    Ok(VlessConfig {
        uuid: uuid.to_string(),
        address: host.to_string(),
        port,
        flow: params.remove("flow"),
        encryption: params.remove("encryption").unwrap_or_else(|| "none".into()),
        security: params.remove("security").unwrap_or_default(),
        sni: params.remove("sni").unwrap_or_default(),
        public_key: params.remove("pbk").unwrap_or_default(),
        short_id: params.remove("sid").unwrap_or_default(),
        spider_x: params.remove("spx").unwrap_or_else(|| "/".into()),
        fingerprint: params.remove("fp").unwrap_or_else(|| "chrome".into()),
        network: params.remove("type").unwrap_or_else(|| "tcp".into()),
        name,
    })
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((k.to_string(), percent_decode(v)))
        })
        .collect()
}

// ── 工具函数 ───────────────────────────────────────────────

/// base64 解码（同时支持标准和 URL-safe 字母表，容忍缺失 padding）。
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    let input = input.trim().trim_end_matches('=');
    let mut buf = Vec::with_capacity(input.len() * 3 / 4 + 2);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;

    for &b in input.as_bytes() {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'\n' | b'\r' | b' ' => continue,
            _ => bail!("base64 中含非法字符: 0x{b:02x}"),
        };
        acc = (acc << 6) | val as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            buf.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(buf)
}

/// 简易 percent-decode（%XX → 字节），正确处理多字节 UTF-8。
fn percent_decode(s: &str) -> String {
    let mut raw = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let h = bytes.next().and_then(hex_val);
            let l = bytes.next().and_then(hex_val);
            if let (Some(hi), Some(lo)) = (h, l) {
                raw.push(hi << 4 | lo);
            }
        } else if b == b'+' {
            raw.push(b' ');
        } else {
            raw.push(b);
        }
    }
    String::from_utf8(raw).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_vless_reality_url() {
        let input = "uuid-1234@example.com:443?type=tcp&security=reality\
                      &pbk=PUBKEY&fp=chrome&sni=www.example.com\
                      &sid=abcd&spx=%2F&flow=xtls-rprx-vision#MyProxy";
        let cfg = parse_vless(input).unwrap();
        assert_eq!(cfg.uuid, "uuid-1234");
        assert_eq!(cfg.address, "example.com");
        assert_eq!(cfg.port, 443);
        assert_eq!(cfg.security, "reality");
        assert_eq!(cfg.public_key, "PUBKEY");
        assert_eq!(cfg.sni, "www.example.com");
        assert_eq!(cfg.short_id, "abcd");
        assert_eq!(cfg.spider_x, "/");
        assert_eq!(cfg.flow.as_deref(), Some("xtls-rprx-vision"));
        assert_eq!(cfg.name, "MyProxy");
    }

    #[test]
    fn base64_standard_and_urlsafe() {
        let encoded = "SGVsbG8gV29ybGQ";
        let decoded = base64_decode(encoded).unwrap();
        assert_eq!(decoded, b"Hello World");

        // With padding
        let encoded2 = "SGVsbG8gV29ybGQ=";
        let decoded2 = base64_decode(encoded2).unwrap();
        assert_eq!(decoded2, b"Hello World");
    }

    #[test]
    fn generate_config_json() {
        let cfg = VlessConfig {
            uuid: "test-uuid".into(),
            address: "1.2.3.4".into(),
            port: 443,
            flow: Some("xtls-rprx-vision".into()),
            encryption: "none".into(),
            network: "tcp".into(),
            security: "reality".into(),
            sni: "www.example.com".into(),
            public_key: "PUBKEY".into(),
            short_id: "abcd".into(),
            spider_x: "/".into(),
            fingerprint: "chrome".into(),
            name: "test".into(),
        };
        let json = generate_config(&cfg, 10808);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["inbounds"][0]["port"], 10808);
        assert_eq!(parsed["outbounds"][0]["settings"]["vnext"][0]["users"][0]["id"], "test-uuid");
        assert_eq!(parsed["outbounds"][0]["settings"]["vnext"][0]["users"][0]["flow"], "xtls-rprx-vision");
        assert_eq!(parsed["outbounds"][0]["streamSettings"]["realitySettings"]["publicKey"], "PUBKEY");
    }
}
