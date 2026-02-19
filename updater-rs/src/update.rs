// ============================================================
// update.rs — 更新协调器
// ============================================================
// 这是更新逻辑的核心。它按顺序执行三个阶段：
//   阶段 1: 检查版本差异
//   阶段 2: 安装新版本 MC + Fabric（如果需要）
//   阶段 3: 同步模组和配置
//
// 通过回调函数 (callback) 向 GUI 报告进度。
// ============================================================

use anyhow::Result;
use std::path::Path;

use crate::fabric;
use crate::packwiz;
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
    fn new(percent: u32, message: impl Into<String>) -> Self {
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
    // 阶段 1: 检查版本
    // ─────────────────────────────────────────────
    on_progress(Progress::new(5, "正在检查更新..."));

    // 尝试拉取远程版本信息
    let remote = match version::fetch_remote_version() {
        Ok(v) => v,
        Err(e) => {
            // 网络失败 → 离线模式
            eprintln!("网络检查失败，进入离线模式: {:#}", e);
            on_progress(Progress::new(100, "离线模式 — 跳过更新"));
            return Ok(UpdateResult::Offline);
        }
    };

    // 读取本地版本
    let local = version::read_local_version(base_dir);

    on_progress(Progress::new(15, format!(
        "远程版本: MC {} / Fabric {}",
        remote.mc_version, remote.fabric_version
    )));

    // ─────────────────────────────────────────────
    // 阶段 2: 大版本升级（如果需要）
    // ─────────────────────────────────────────────
    if version::needs_version_upgrade(&remote, &local) {
        on_progress(Progress::new(20, format!(
            "正在升级到 MC {} ...",
            remote.mc_version
        )));

        // 2a. 安装新版本 Fabric
        on_progress(Progress::new(25, "正在安装 Fabric..."));
        fabric::install_fabric(base_dir, &remote.mc_version, &remote.fabric_version)?;

        // 2b. 清理旧版本目录
        on_progress(Progress::new(40, "正在清理旧版本..."));
        fabric::cleanup_old_versions(base_dir, &remote.version_tag)?;

        // 2c. 清空旧模组（新版本模组由 packwiz 重新下载）
        on_progress(Progress::new(50, "正在清理旧模组..."));
        fabric::clean_mods_dir(base_dir)?;

        // 2d. 保存新的本地版本记录
        let new_local = version::LocalVersion {
            mc_version: remote.mc_version.clone(),
            fabric_version: remote.fabric_version.clone(),
            version_tag: remote.version_tag.clone(),
        };
        version::save_local_version(base_dir, &new_local)?;

        on_progress(Progress::new(55, "版本升级完成"));
    } else {
        on_progress(Progress::new(55, "版本已是最新"));
    }

    // ─────────────────────────────────────────────
    // 阶段 3: 同步模组和配置
    // ─────────────────────────────────────────────
    on_progress(Progress::new(60, "正在同步模组..."));

    packwiz::sync_modpack(base_dir, &remote.pack_url)?;

    on_progress(Progress::new(95, "模组同步完成"));

    // 完成
    on_progress(Progress::new(100, "更新完成，正在启动游戏..."));

    Ok(UpdateResult::Success)
}
