// ============================================================
// selfupdate.rs — 更新器自更新模块
// ============================================================
// 负责：
//   1. 计算当前 exe 的 SHA256
//   2. 对比远程 server.json 中的 updater_sha256
//   3. 如果不同，下载新 exe → 替换自身 → 重启
//   4. 清理旧版 exe 残留 (.old)
//
// Windows 上正在运行的 exe 不能直接覆盖，但可以重命名。
// 策略：旧 exe → rename .old → 新 exe 写入原路径 → 重启。
// ============================================================

use anyhow::{Context, Result};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config;

/// 自更新检查结果
pub enum SelfUpdateResult {
    /// 无需更新，继续正常流程
    UpToDate,
    /// 已下载新版并重启，调用方应立即退出
    Restarting,
}

/// 获取当前 exe 的路径
fn current_exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("无法获取当前 exe 路径")
}

/// 清理上次自更新留下的 .old 文件
pub fn cleanup_old_exe() {
    if let Ok(exe) = current_exe_path() {
        let old = exe.with_extension("exe.old");
        if old.exists() {
            // 可能上次更新后重启的，删掉旧版
            let _ = fs::remove_file(&old);
        }
    }
}

/// 计算文件的 SHA256 哈希值（小写十六进制）
fn sha256_file(path: &Path) -> Result<String> {
    use sha2::Digest;
    use std::io::BufReader;

    let file = fs::File::open(path)
        .with_context(|| format!("打开文件失败: {}", path.display()))?;
    let mut reader = BufReader::new(file);

    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = reader.read(&mut buf).context("读取文件失败")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 检查并执行自更新。
///
/// 返回 `SelfUpdateResult::Restarting` 时，调用方应立即退出进程。
pub fn check_and_update(
    updater_url: Option<&str>,
    updater_sha256: Option<&str>,
    on_progress: &dyn Fn(crate::update::Progress),
) -> Result<SelfUpdateResult> {
    // 如果没有配置自更新 URL 或哈希，或哈希为空字符串，跳过
    let (url, expected_hash) = match (updater_url, updater_sha256) {
        (Some(u), Some(h)) if !u.is_empty() && !h.is_empty() => (u, h),
        _ => return Ok(SelfUpdateResult::UpToDate),
    };

    let exe_path = current_exe_path()?;

    on_progress(crate::update::Progress::new(1, "检查更新器版本..."));

    // 计算当前 exe 的哈希
    let current_hash = sha256_file(&exe_path)?;

    if current_hash == expected_hash {
        return Ok(SelfUpdateResult::UpToDate);
    }

    on_progress(crate::update::Progress::new(2, "发现更新器新版本，正在下载..."));

    // 下载新 exe 到临时文件
    let temp_path = exe_path.with_extension("exe.new");

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(config::DOWNLOAD_TIMEOUT_SECS))
        .build();

    let response = agent
        .get(url)
        .call()
        .context("下载更新器新版本失败")?;

    // 获取文件大小
    let total_size = response
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    let mut reader = response.into_reader();
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
                    format!("下载更新器... {:.1}/{:.1} MB", mb_done, mb_total),
                ));
            }
        }
    }
    drop(file);

    // 验证下载的文件哈希
    let new_hash = sha256_file(&temp_path)?;
    if new_hash != expected_hash {
        let _ = fs::remove_file(&temp_path);
        anyhow::bail!(
            "更新器下载校验失败\n\
             预期: {}\n\
             实际: {}",
            expected_hash,
            new_hash
        );
    }

    on_progress(crate::update::Progress::new(10, "正在替换更新器..."));

    // 替换流程：旧 exe → .old，新 exe → 原路径
    let old_path = exe_path.with_extension("exe.old");

    // 删除可能残留的旧 .old
    if old_path.exists() {
        fs::remove_file(&old_path).ok();
    }

    // 重命名当前运行的 exe（Windows 允许重命名正在运行的 exe）
    fs::rename(&exe_path, &old_path)
        .context("重命名旧版更新器失败")?;

    // 移动新 exe 到原路径
    if let Err(e) = fs::rename(&temp_path, &exe_path) {
        // 回滚：把旧的移回去
        let _ = fs::rename(&old_path, &exe_path);
        return Err(e).context("替换更新器失败");
    }

    on_progress(crate::update::Progress::new(11, "更新器已更新，正在重启..."));

    // 启动新版 exe
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    std::process::Command::new(&exe_path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("启动新版更新器失败")?;

    Ok(SelfUpdateResult::Restarting)
}
