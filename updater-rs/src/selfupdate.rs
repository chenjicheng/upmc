// ============================================================
// selfupdate.rs — 更新器自更新模块
// ============================================================
// 负责：
//   1. 读取当前 exe 内嵌的版本号
//   2. 从版本信息 URL 获取最新版本和下载链接
//      - Stable: upmc.chenjicheng.cn/version.json（语义化版本比较）
//      - Dev:    upmc.chenjicheng.cn/dev/version.json（build_id 比较）
//   3. 如果远程版本更高/不同，下载新 exe → 委托 PowerShell 替换并重启
//   4. 清理残留临时文件
//
// 自替换策略（PowerShell 外部脚本）：
//   当前进程下载新 exe → .exe.new
//   → 生成 PowerShell 脚本：等当前 PID 退出 → 覆盖 exe → 启动新版
//   → 当前进程退出
//   完全由外部进程完成替换，避免 Windows 文件锁问题。
// ============================================================

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{self, ChannelConfig, UpdateChannel};
use crate::retry;

/// 当前更新器版本（编译时从 Cargo.toml 读取）
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 自更新检查结果
pub enum SelfUpdateResult {
    /// 无需更新，继续正常流程
    UpToDate,
    /// 已下载新版并委托 PowerShell 替换，调用方应立即退出
    Restarting,
}

/// 获取当前 exe 的路径
fn current_exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("无法获取当前 exe 路径")
}

/// 清理上次自更新残留的临时文件（.new）。
///
/// 新进程启动时调用。PowerShell 脚本正常流程会自行清理，
/// 但如果中途被杀或出错，这里兜底清理。
pub fn cleanup_old_exe() {
    if let Ok(exe) = current_exe_path() {
        let new = exe.with_extension("exe.new");
        if new.exists() {
            let _ = fs::remove_file(&new);
        }
        // 兼容旧版自更新策略可能残留的 .old 文件
        let old = exe.with_extension("exe.old");
        if old.exists() {
            let _ = fs::remove_file(&old);
        }
    }
}

/// 解析语义化版本号为 (major, minor, patch) 元组。
///
/// 支持格式如 "0.1.0"、"1.2.3"。无法解析时返回 None。
fn parse_semver(version: &str) -> Option<(u64, u64, u64)> {
    let parts: Vec<&str> = version.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let major = parts[0].parse::<u64>().ok()?;
    let minor = parts[1].parse::<u64>().ok()?;
    let patch = parts[2].parse::<u64>().ok()?;
    Some((major, minor, patch))
}

/// 判断远程版本是否比本地版本更高。
///
/// 如果任一版本号无法解析，拒绝更新（防止格式错误的版本号触发意外下载）。
fn is_remote_newer(current: &str, remote: &str) -> bool {
    match (parse_semver(current), parse_semver(remote)) {
        (Some(cur), Some(rem)) => rem > cur,
        _ => {
            eprintln!("版本号解析失败，跳过自更新 (current={current:?}, remote={remote:?})");
            false
        }
    }
}

/// 更新器远程版本信息（从版本信息 URL 获取）
#[derive(Debug, Deserialize)]
pub struct UpdaterVersionInfo {
    /// 最新版本号，如 "0.3.5"
    pub version: String,
    /// exe 下载地址（经 ghfast 代理）
    pub download_url: String,
    /// 构建 ID（7 位 commit SHA），仅 dev 通道使用
    #[serde(default)]
    pub build_id: Option<String>,
}

/// 从版本信息 URL 获取更新器版本信息（带重试）。
fn fetch_updater_info(channel: UpdateChannel) -> Result<UpdaterVersionInfo> {
    retry::with_retry(
        config::RETRY_MAX_ATTEMPTS,
        config::RETRY_BASE_DELAY_SECS,
        "获取更新器版本信息",
        || fetch_updater_info_inner(channel),
    )
}

/// fetch_updater_info 的内部实现（单次尝试）。
fn fetch_updater_info_inner(channel: UpdateChannel) -> Result<UpdaterVersionInfo> {
    let url = config::updater_version_url(channel);

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(config::HTTP_TIMEOUT_SECS)))
        .build()
        .into();

    let body = agent
        .get(url)
        .call()
        .context("无法连接到更新器版本服务器")?;

    let text = body
        .into_body()
        .read_to_string()
        .context("读取版本信息失败")?;

    serde_json::from_str(&text).context("解析 version.json 失败")
}

/// 检查并执行自更新。
///
/// 根据通道从对应的 version.json 获取最新版本和下载链接，
/// 与 server.json 完全解耦。
///
/// - Stable: 使用语义化版本比较
/// - Dev: 使用 build_id（commit SHA）比较，不同即更新
///
/// 返回 `SelfUpdateResult::Restarting` 时，调用方应立即退出进程。
pub fn check_and_update(
    base_dir: &Path,
    channel_config: &ChannelConfig,
    on_progress: &dyn Fn(crate::update::Progress),
) -> Result<SelfUpdateResult> {
    let channel = channel_config.channel;
    let channel_label = match channel {
        UpdateChannel::Stable => "stable",
        UpdateChannel::Dev => "dev",
    };
    on_progress(crate::update::Progress::new(
        1,
        format!("检查更新器版本 ({channel_label})..."),
    ));

    // 从对应通道的 version.json 获取版本信息
    let info = fetch_updater_info(channel)?;
    let url = &info.download_url;

    // 根据通道判断是否需要更新
    let needs_update = match channel {
        UpdateChannel::Stable => {
            // 稳定通道：语义化版本比较
            is_remote_newer(CURRENT_VERSION, &info.version)
        }
        UpdateChannel::Dev => {
            // 开发通道：比较 build_id
            match (&info.build_id, &channel_config.dev_build_id) {
                (Some(remote_id), Some(local_id)) => remote_id != local_id,
                (Some(_), None) => true, // 本地无 build_id，需要更新
                _ => false,              // 远程无 build_id，跳过
            }
        }
    };

    if !needs_update {
        return Ok(SelfUpdateResult::UpToDate);
    }

    let progress_msg = match channel {
        UpdateChannel::Stable => format!(
            "发现更新器新版本 {} → {}，正在下载...",
            CURRENT_VERSION, &info.version
        ),
        UpdateChannel::Dev => {
            let remote_id = info.build_id.as_deref().unwrap_or("unknown");
            let local_id = channel_config.dev_build_id.as_deref().unwrap_or("none");
            format!("发现新的开发构建 {local_id} → {remote_id}，正在下载...")
        }
    };
    on_progress(crate::update::Progress::new(2, progress_msg));

    // 下载新 exe 到临时文件
    let exe_path = current_exe_path()?;
    let temp_path = exe_path.with_extension("exe.new");

    // 清理上次可能残留的临时文件
    if temp_path.exists() {
        fs::remove_file(&temp_path).ok();
    }

    // 下载 + 校验：用闭包包裹，出错时统一清理临时文件
    let download_and_verify = || -> Result<()> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(config::DOWNLOAD_TIMEOUT_SECS)))
            .build()
            .into();

        let response = agent
            .get(url)
            .call()
            .context("下载更新器新版本失败")?;

        // 获取文件大小
        let total_size = response
            .body()
            .content_length()
            .unwrap_or(0);

        let mut reader = response.into_body().into_reader();
        let mut file = fs::File::create(&temp_path)
            .context("创建临时文件失败")?;

        let mut buf = [0u8; 65536];
        let mut downloaded: u64 = 0;
        {
            use std::io::Write;
            loop {
                let n = reader.read(&mut buf).context("读取下载数据失败")?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n]).context("写入文件失败")?;
                downloaded += n as u64;

                if total_size > 0 {
                    let fraction = downloaded as f64 / total_size as f64;
                    let pct = 2 + (fraction * 8.0) as u32; // 2% ~ 10%
                    let mb_done = downloaded as f64 / 1_048_576.0;
                    let mb_total = total_size as f64 / 1_048_576.0;
                    on_progress(crate::update::Progress::new(
                        pct.min(10),
                        format!("下载更新器... {mb_done:.1}/{mb_total:.1} MB"),
                    ));
                }
            }
        }
        drop(file);

        // 基本完整性校验：检查文件大小不为 0 且是有效的 PE 文件
        let file_size = fs::metadata(&temp_path)
            .context("读取下载文件信息失败")?
            .len();
        if file_size == 0 {
            anyhow::bail!("下载的更新器文件为空");
        }
        // 检查 PE 文件头 (MZ magic)
        {
            let mut f = fs::File::open(&temp_path).context("打开下载文件失败")?;
            let mut magic = [0u8; 2];
            if f.read_exact(&mut magic).is_err() || &magic != b"MZ" {
                anyhow::bail!("下载的文件不是有效的可执行文件");
            }
        }
        Ok(())
    };

    let result = retry::with_retry(
        config::RETRY_MAX_ATTEMPTS,
        config::RETRY_BASE_DELAY_SECS,
        "下载更新器",
        download_and_verify,
    );

    if let Err(e) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(e);
    }

    on_progress(crate::update::Progress::new(10, "正在准备替换更新器..."));

    // Dev 通道：更新 channel.json 中的 build_id
    if channel == UpdateChannel::Dev {
        if let Some(ref new_build_id) = info.build_id {
            let mut cfg = config::read_channel_config(base_dir);
            cfg.dev_build_id = Some(new_build_id.clone());
            if let Err(e) = config::save_channel_config(base_dir, &cfg) {
                eprintln!("保存 dev build_id 失败: {e:#}");
            }
        }
    }

    // ── 委托 PowerShell 完成替换 ──
    //
    // 生成内联 PowerShell 脚本：
    //   1. 等待当前进程 (PID) 退出（最多 30 秒）
    //   2. 尝试用 .new 文件覆盖原 exe（最多重试 3 次，间隔 1 秒）
    //   3. 成功时删除 .new 临时文件
    //   4. 无论替换是否成功都启动 exe（失败时回退到旧版）
    //
    // 这样当前进程可以安全退出，由外部 PowerShell 进程完成文件替换，
    // 彻底避免 Windows 文件锁问题。

    let current_pid = std::process::id();
    let exe_path_str = exe_path.to_string_lossy();
    let temp_path_str = temp_path.to_string_lossy();

    // PowerShell 脚本（单行内联，通过 -Command 传入）
    //
    // Copy-Item 含重试逻辑：杀毒软件、索引服务可能短暂锁住文件，
    // 重试 3 次（间隔 1s）后仍失败则保留 .new 文件留给下次启动清理。
    // 无论替换是否成功，都启动 exe 以确保用户不会面对"什么都没启动"。
    let ps_script = format!(
        r#"$ErrorActionPreference='Stop'; try {{ $p=Get-Process -Id {pid} -ErrorAction SilentlyContinue; if($p) {{ $p.WaitForExit(30000) | Out-Null }} }} catch {{}}; Start-Sleep -Milliseconds 500; $ok=$false; for($i=0;$i -lt 3;$i++) {{ try {{ Copy-Item -Path '{new}' -Destination '{exe}' -Force; $ok=$true; break }} catch {{ Start-Sleep -Seconds 1 }} }}; if($ok) {{ Remove-Item -Path '{new}' -Force -ErrorAction SilentlyContinue }}; Start-Process -FilePath '{exe}'"#,
        pid = current_pid,
        new = temp_path_str.replace('\'', "''"),
        exe = exe_path_str.replace('\'', "''"),
    );

    std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-Command", &ps_script])
        .creation_flags(config::CREATE_NO_WINDOW)
        .spawn()
        .context("启动 PowerShell 替换脚本失败")?;

    on_progress(crate::update::Progress::new(11, "更新器已更新，正在重启..."));

    Ok(SelfUpdateResult::Restarting)
}
