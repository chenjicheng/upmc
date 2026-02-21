// ============================================================
// update.rs — 更新协调器
// ============================================================
// 完整流程（四个阶段）：
//   阶段 0: 首次安装自举（下载 JRE、PCL2、工具 jar）
//   阶段 1: 检查版本差异
//   阶段 2: 安装新版本 MC + Fabric（如果需要）
//   阶段 3: 同步模组和配置
//
// 通过回调函数 (callback) 向 GUI 报告进度。
// ============================================================

use anyhow::{bail, Result};
use std::path::Path;

use crate::bootstrap;
use crate::fabric;
use crate::packwiz;
use crate::selfupdate;
use crate::version;

/// 更新进度信息，传给 GUI 显示。
#[derive(Debug, Clone, Default)]
pub struct Progress {
    /// 进度百分比 (0-100)
    pub percent: u32,
    /// 当前状态描述文字
    pub message: String,
}

impl Progress {
    pub fn new(percent: u32, message: impl Into<String>) -> Self {
        Self {
            percent,
            message: message.into(),
        }
    }
}

/// 更新结果枚举
pub enum UpdateResult {
    /// 更新成功完成
    Success,
    /// 网络不可用，跳过更新（离线模式）
    Offline,
    /// 更新器自身已更新并重启，当前进程应直接退出（不启动 PCL2）
    SelfUpdateRestarting,
}

/// 执行完整的更新流程。
///
/// # 参数
/// - `base_dir`: 更新器 exe 所在的根目录
/// - `on_progress`: 进度回调函数，每个阶段都会调用
///
/// # 返回
/// - `Ok(UpdateResult::Success)` — 更新完成
/// - `Ok(UpdateResult::Offline)` — 离线模式，跳过更新
/// - `Err(...)` — 更新过程中出错
pub fn run_update(
    base_dir: &Path,
    on_progress: &dyn Fn(Progress),
) -> Result<UpdateResult> {
    // ─────────────────────────────────────────────
    // 阶段 0+1: 拉取远程版本 + 首次安装
    // ─────────────────────────────────────────────
    on_progress(Progress::new(1, "正在连接更新服务器..."));

    // 尝试拉取远程版本信息
    let remote = match version::fetch_remote_version() {
        Ok(v) => v,
        Err(e) => {
            // 网络失败：检查是否已安装过
            if bootstrap::is_bootstrapped(base_dir) {
                // 已安装 → 离线模式，跳过更新直接启动
                crate::logging::write(format!("网络检查失败，进入离线模式: {e:#}"));
                on_progress(Progress::new(100, "离线模式 — 跳过更新"));
                return Ok(UpdateResult::Offline);
            }
            // 未安装 → 无法继续，首次运行需要网络
            bail!(
                "首次运行需要网络连接来下载必要组件。\n\
                 请检查网络后重试。\n\n\
                 错误详情: {e:#}"
            );
        }
    };

    // ─────────────────────────────────────────────
    // 阶段 -1: 检查更新器自身是否需要更新
    // ─────────────────────────────────────────────
    match selfupdate::check_and_update(
        remote.downloads.updater_url.as_deref(),
        remote.downloads.updater_version.as_deref(),
        on_progress,
    ) {
        Ok(selfupdate::SelfUpdateResult::Restarting) => {
            // 新版已下载并启动，当前进程应直接退出（不启动 PCL2）
            return Ok(UpdateResult::SelfUpdateRestarting);
        }
        Ok(selfupdate::SelfUpdateResult::UpToDate) => {
            // 不需要更新，继续
        }
        Err(e) => {
            // 自更新失败不阻塞，记录日志继续
            crate::logging::write(format!("自更新检查失败（不影响正常使用）: {e:#}"));
        }
    }

    // ─────────────────────────────────────────────
    // 阶段 0: 首次安装自举（如果需要）
    // ─────────────────────────────────────────────
    if bootstrap::needs_bootstrap(base_dir) {
        on_progress(Progress::new(2, "首次运行，正在下载组件..."));
        bootstrap::run_bootstrap(base_dir, &remote.downloads, on_progress)?;
    } else {
        on_progress(Progress::new(50, "组件检查完毕"));
    }

    // ─────────────────────────────────────────────
    // 阶段 1: 检查版本
    // ─────────────────────────────────────────────
    let local = version::read_local_version(base_dir);

    on_progress(Progress::new(55, format!(
        "远程版本: MC {} / Fabric {}",
        remote.mc_version, remote.fabric_version
    )));

    // ─────────────────────────────────────────────
    // 阶段 2: 大版本升级（如果需要）
    // ─────────────────────────────────────────────
    if version::needs_version_upgrade(&remote, &local) {
        on_progress(Progress::new(58, format!(
            "正在升级到 MC {} ...",
            remote.mc_version
        )));

        // 2a. 安装新版本 Fabric
        on_progress(Progress::new(60, "正在安装 Fabric..."));
        fabric::install_fabric(base_dir, &remote.mc_version, &remote.fabric_version)?;

        // 2b. 清理旧版本目录
        on_progress(Progress::new(70, "正在清理旧版本..."));
        fabric::cleanup_old_versions(base_dir, &remote.version_tag)?;

        // 2c. 清空旧模组（新版本模组由 packwiz 重新下载）
        on_progress(Progress::new(75, "正在清理旧模组..."));
        fabric::clean_mods_dir(base_dir)?;

        // 2d. 保存新的本地版本记录
        let new_local = version::LocalVersion {
            mc_version: remote.mc_version.clone(),
            fabric_version: remote.fabric_version.clone(),
            version_tag: remote.version_tag.clone(),
        };
        version::save_local_version(base_dir, &new_local)?;

        on_progress(Progress::new(78, "版本升级完成"));
    } else {
        on_progress(Progress::new(78, "版本已是最新"));
    }

    // ── 确保原版 MC 客户端已下载（每次启动都检查） ──
    // 这是一个幂等操作：如果文件已存在会立即跳过
    on_progress(Progress::new(79, "检查原版 MC 客户端..."));
    fabric::ensure_vanilla_client(base_dir, &remote.mc_version)?;

    // ── 修正 PCL2 版本隔离设置 ──
    // PCL2 会在版本目录下自动创建 Setup.ini 并启用隔离，
    // 导致游戏目录指向 versions/<tag>/ 而非 .minecraft/，
    // 每次启动前都需要修正为不隔离。
    on_progress(Progress::new(79, "修正版本隔离设置..."));
    fabric::fix_version_isolation(base_dir, &remote.version_tag)?;

    // ─────────────────────────────────────────────
    // 阶段 3: 同步模组和配置
    // ─────────────────────────────────────────────
    on_progress(Progress::new(80, "正在同步模组..."));

    packwiz::sync_modpack(base_dir, &remote.pack_url)?;

    on_progress(Progress::new(95, "模组同步完成"));

    // 完成
    on_progress(Progress::new(100, "更新完成，正在启动游戏..."));

    Ok(UpdateResult::Success)
}
