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
mod packwiz;
mod update;
mod version;

use std::env;
use std::path::PathBuf;

fn main() {
    // 获取 exe 所在的目录作为基准路径
    // 所有相对路径（.minecraft、jre、PCL 等）都相对于这个目录
    let base_dir = get_base_dir();

    // 启动 GUI（内部会开后台线程执行更新）
    gui::UpdaterApp::run(base_dir);
}

/// 获取当前 exe 文件所在的目录。
///
/// 这样无论玩家把文件夹放在哪里，路径都能正确解析。
/// 例如：如果 exe 在 `D:\Games\我的服务器\我的服务器.exe`，
/// 返回 `D:\Games\我的服务器\`。
fn get_base_dir() -> PathBuf {
    env::current_exe()
        .expect("无法获取 exe 路径")
        .parent()
        .expect("无法获取 exe 所在目录")
        .to_path_buf()
}
