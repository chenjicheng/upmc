// ============================================================
// selfupdate.rs — 更新器自更新模块
// ============================================================
// 负责：
//   1. 从版本信息 URL 获取最新的 build_id 和下载链接
//      - Stable: upmc.chenjicheng.cn/version.json
//      - Dev:    upmc.chenjicheng.cn/dev/version.json
//   2. 对比编译期硬编码的 build_id（commit SHA）与远程 build_id
//   3. 如果不同，下载新 exe → 启动自拷贝 helper 替换并重启
//   4. 清理残留临时文件
//
// 所有通道统一使用 build_id（commit SHA）判断是否需要更新，
// 不区分通道、不使用 semver 比较，逻辑简单可靠。
//
// 自替换策略（自拷贝 helper）：
//   当前进程下载新 exe → .exe.new
//   → 将当前 exe 复制为 upmc-update-helper.exe
//   → helper 进程等待原 exe 解锁后覆盖 exe 并启动新版
//   → 当前进程退出
//
// 该策略避免调用 PowerShell / cmd / 脚本解释器，也不使用
// ExecutionPolicy Bypass，降低 Defender 启发式误报概率。
// ============================================================

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::config::{self, UpdateChannel};
use crate::retry;

/// 当前构建 ID（CI 编译时注入的 commit SHA）
/// 本地开发时为 None
const CURRENT_BUILD_ID: Option<&str> = option_env!("UPMC_BUILD_ID");

/// helper 模式参数。主程序启动时如果检测到该参数，则只执行自更新替换逻辑。
const SELF_UPDATE_HELPER_ARG: &str = "--apply-self-update";
const SELF_UPDATE_SOURCE_ARG: &str = "--source";
const SELF_UPDATE_TARGET_ARG: &str = "--target";
const SELF_UPDATE_RESTART_ARG: &str = "--restart";
const SELF_UPDATE_HELPER_NAME: &str = "upmc-update-helper.exe";

/// 自更新检查结果
pub enum SelfUpdateResult {
    /// 无需更新，继续正常流程
    UpToDate,
    /// 已下载新版并委托 helper 替换，调用方应立即退出
    Restarting,
}

/// 获取当前 exe 的路径
fn current_exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("无法获取当前 exe 路径")
}

/// 如果当前进程是自更新 helper，则执行替换流程并返回 true。
///
/// 该函数必须在 main() 最开始调用，避免 helper 初始化 GUI 或执行正常更新流程。
pub fn try_run_update_helper_from_args() -> Result<bool> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some(SELF_UPDATE_HELPER_ARG) {
        return Ok(false);
    }

    let mut source: Option<PathBuf> = None;
    let mut target: Option<PathBuf> = None;
    let mut restart: Option<PathBuf> = None;

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            SELF_UPDATE_SOURCE_ARG => {
                source = args.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            SELF_UPDATE_TARGET_ARG => {
                target = args.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            SELF_UPDATE_RESTART_ARG => {
                restart = args.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }

    let source = source.context("自更新 helper 缺少 --source 参数")?;
    let target = target.context("自更新 helper 缺少 --target 参数")?;
    let restart = restart.unwrap_or_else(|| target.clone());

    apply_downloaded_update(&source, &target, &restart)?;
    Ok(true)
}

/// 清理上次自更新残留的临时文件（.new / .old / helper）。
///
/// 新进程启动时调用。helper 正常流程会自行清理 .new，
/// 但如果中途被杀、杀毒软件短暂锁定或系统重启，这里兜底清理。
pub fn cleanup_old_exe() {
    if let Ok(exe) = current_exe_path() {
        let new = exe.with_extension("exe.new");
        if new.exists() {
            if let Err(e) = fs::remove_file(&new) {
                eprintln!("清理残留 .exe.new 失败: {e}");
            }
        }

        // 兼容旧版自更新策略可能残留的 .old 文件
        let old = exe.with_extension("exe.old");
        if old.exists() {
            if let Err(e) = fs::remove_file(&old) {
                eprintln!("清理残留 .exe.old 失败: {e}");
            }
        }

        // 清理上次复制出来的 helper。Windows 下 helper 运行时无法删除自身，
        // 因此通常会在新版启动时由主程序清理。
        if let Some(parent) = exe.parent() {
            let helper = parent.join(SELF_UPDATE_HELPER_NAME);
            if helper.exists() {
                if let Err(e) = fs::remove_file(&helper) {
                    eprintln!("清理残留 helper 失败: {e}");
                }
            }
        }
    }
}

/// 更新器远程版本信息（从版本信息 URL 获取）
#[derive(Debug, Deserialize)]
pub struct UpdaterVersionInfo {
    /// exe 下载地址（经 gh.cjcx.org 代理）
    pub download_url: String,
    /// 构建 ID（commit SHA），所有通道统一使用
    #[serde(default)]
    pub build_id: Option<String>,
    /// exe 文件的 SHA256 哈希（小写十六进制），用于下载后完整性校验
    #[serde(default)]
    pub sha256: Option<String>,
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

    let agent = config::http_agent();

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
/// 所有通道统一使用 build_id（commit SHA）判断是否需要更新：
///   本地 build_id != 远程 build_id → 需要更新
///
/// 返回 `SelfUpdateResult::Restarting` 时，调用方应立即退出进程。
pub fn check_and_update(
    channel: UpdateChannel,
    on_progress: &dyn Fn(crate::update::Progress),
) -> Result<SelfUpdateResult> {
    on_progress(crate::update::Progress::new(
        1,
        format!("检查更新器版本 ({channel})..."),
    ));

    // 从对应通道的 version.json 获取版本信息
    let info = fetch_updater_info(channel)?;

    // 统一用 build_id 判断是否需要更新
    let needs_update = match (&info.build_id, CURRENT_BUILD_ID) {
        (Some(remote_id), Some(local_id)) => remote_id != local_id,
        (Some(_), None) => true, // 本地无 build_id（非 CI 构建），需要更新
        _ => false,              // 远程无 build_id，跳过
    };

    if !needs_update {
        return Ok(SelfUpdateResult::UpToDate);
    }

    let local_id = CURRENT_BUILD_ID.unwrap_or("local");
    let remote_id = info.build_id.as_deref().unwrap_or("unknown");
    on_progress(crate::update::Progress::new(
        2,
        format!("发现新版本 {local_id} → {remote_id}，正在下载..."),
    ));

    // 下载新 exe 到临时文件
    let exe_path = current_exe_path()?;
    let temp_path = exe_path.with_extension("exe.new");
    let download_url = &info.download_url;

    // 校验下载 URL 必须使用 HTTPS
    if !download_url.starts_with("https://") {
        bail!("更新器下载 URL 必须使用 HTTPS 协议: {download_url}");
    }

    // 清理上次可能残留的临时文件
    if temp_path.exists() {
        fs::remove_file(&temp_path).ok();
    }

    // 保存 sha256 供校验使用
    let expected_sha256 = info.sha256.clone();

    // 下载 + 校验：用闭包包裹，出错时统一清理临时文件
    let download_and_verify = || -> Result<()> {
        let agent = config::download_agent();

        let response = agent
            .get(download_url)
            .call()
            .context("下载更新器新版本失败")?;

        // 获取文件大小
        let total_size = response.body().content_length().unwrap_or(0);

        let mut reader = response.into_body().into_reader();
        let mut file = fs::File::create(&temp_path).context("创建临时文件失败")?;

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
            bail!("下载的更新器文件为空");
        }
        // 检查 PE 文件头 (MZ magic)
        {
            let mut f = fs::File::open(&temp_path).context("打开下载文件失败")?;
            let mut magic = [0u8; 2];
            if f.read_exact(&mut magic).is_err() || &magic != b"MZ" {
                bail!("下载的文件不是有效的可执行文件");
            }
        }

        // SHA256 校验
        match &expected_sha256 {
            Some(expected) => {
                use sha2::{Digest, Sha256};
                let file_bytes = fs::read(&temp_path).context("读取下载文件用于校验失败")?;
                let actual = format!("{:x}", Sha256::digest(&file_bytes));
                // 统一小写比较，兼容服务端返回大写哈希
                if actual != expected.to_lowercase() {
                    bail!(
                        "SHA256 校验失败！文件可能被篡改。\n\
                         期望: {expected}\n\
                         实际: {actual}"
                    );
                }
            }
            None => {
                eprintln!("[安全警告] version.json 未提供 sha256 字段，跳过完整性校验");
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

    spawn_update_helper(&exe_path, &temp_path).context("启动自更新 helper 失败")?;

    on_progress(crate::update::Progress::new(11, "更新器已更新，正在重启..."));

    Ok(SelfUpdateResult::Restarting)
}

/// 复制当前 exe 为 helper，并由 helper 完成替换。
fn spawn_update_helper(exe_path: &Path, temp_path: &Path) -> Result<()> {
    let helper_path = exe_path
        .parent()
        .context("无法确定更新器所在目录")?
        .join(SELF_UPDATE_HELPER_NAME);

    if helper_path.exists() {
        fs::remove_file(&helper_path).ok();
    }

    fs::copy(exe_path, &helper_path).with_context(|| {
        format!(
            "创建自更新 helper 失败: {} → {}",
            exe_path.display(),
            helper_path.display()
        )
    })?;

    Command::new(&helper_path)
        .arg(SELF_UPDATE_HELPER_ARG)
        .arg(SELF_UPDATE_SOURCE_ARG)
        .arg(temp_path)
        .arg(SELF_UPDATE_TARGET_ARG)
        .arg(exe_path)
        .arg(SELF_UPDATE_RESTART_ARG)
        .arg(exe_path)
        .spawn()
        .with_context(|| format!("启动自更新 helper 失败: {}", helper_path.display()))?;

    Ok(())
}

/// helper 进程执行的替换逻辑。
///
/// Windows 会锁定正在运行的 exe，因此这里不依赖 PID，也不调用 PowerShell；
/// 只做有限时间重试，直到主进程退出后目标 exe 可写。
fn apply_downloaded_update(source: &Path, target: &Path, restart: &Path) -> Result<()> {
    if !source.exists() {
        bail!("自更新源文件不存在: {}", source.display());
    }

    let mut last_error: Option<std::io::Error> = None;
    let mut copied = false;

    // 主进程收到 Restarting 后会退出 GUI 事件循环。这里最多等待约 30 秒，
    // 兼容杀毒软件、索引服务或文件系统短暂占用。
    for _ in 0..30 {
        match fs::copy(source, target) {
            Ok(_) => {
                copied = true;
                break;
            }
            Err(e) => {
                last_error = Some(e);
                thread::sleep(Duration::from_secs(1));
            }
        }
    }

    if !copied {
        let detail = last_error
            .map(|e| e.to_string())
            .unwrap_or_else(|| "未知错误".to_string());
        bail!(
            "自更新替换失败: {} → {}\n{detail}",
            source.display(),
            target.display()
        );
    }

    // 替换成功后清理 .new。helper 自身通常会在下一次主程序启动时清理。
    fs::remove_file(source).ok();

    Command::new(restart)
        .spawn()
        .with_context(|| format!("启动新版更新器失败: {}", restart.display()))?;

    Ok(())
}
