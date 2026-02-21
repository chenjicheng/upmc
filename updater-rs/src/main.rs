// ============================================================
// main.rs — 程序入口
// ============================================================
// 职责：
//   1. 确定 exe 所在目录作为工作基准路径
//   2. 隐藏控制台窗口（release 模式下）
//   3. 启动 GUI
// ============================================================

// 在 release 模式下隐藏控制台黑框
// 这个属性让 Windows 不会弹出 cmd 窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bootstrap;
mod config;
mod fabric;
mod gui;
mod logging;
mod packwiz;
mod retry;
mod selfupdate;
mod update;
mod version;

use std::env;
use std::path::PathBuf;

fn main() {
    // 初始化日志（写入系统临时目录）
    logging::init();

    // 清理上次自更新残留的临时文件（.new / .old）
    selfupdate::cleanup_old_exe();

    // 获取 exe 所在的目录作为基准路径
    // 所有相对路径（.minecraft、jre、PCL 等）都相对于这个目录
    let base_dir = get_base_dir();

    // 启动 GUI（内部会开后台线程执行更新）
    gui::UpdaterApp::run(base_dir);
}

/// 获取组件安装的基准目录。
///
/// 返回 exe 所在目录下的 `CJC整合包/` 子目录。
/// 例如：如果 exe 在 `D:\Games\我的服务器.exe`，
/// 返回 `D:\Games\CJC整合包\`。
fn get_base_dir() -> PathBuf {
    let exe_dir = env::current_exe()
        .expect("无法获取 exe 路径")
        .parent()
        .expect("无法获取 exe 所在目录")
        .to_path_buf();
    exe_dir.join(config::INSTALL_DIR)
}
