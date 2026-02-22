// ============================================================
// main.rs — 程序入口
// ============================================================
// 职责：
//   1. 确定安装基准路径（用户文档文件夹），并处理旧位置迁移
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
mod retry;
mod selfupdate;
mod update;
mod version;

use std::path::PathBuf;

fn main() {
    // 清理上次自更新残留的临时文件（.new / .old）
    selfupdate::cleanup_old_exe();

    // 获取安装基准路径（用户文档文件夹）
    // 如果旧位置有安装，先迁移到新位置
    let base_dir = get_base_dir();

    // 启动 GUI（内部会开后台线程执行更新）
    gui::UpdaterApp::run(base_dir);
}

/// 获取组件安装的基准目录。
///
/// 返回用户文档文件夹下的 `CJC整合包/` 子目录。
/// 例如：`C:\Users\<用户>\Documents\CJC整合包\`
///
/// 如果检测到旧版安装目录（exe 同级的 CJC整合包/），
/// 会自动将其迁移到文档文件夹。
fn get_base_dir() -> PathBuf {
    let new_dir = config::get_install_dir();
    let legacy_dir = config::get_legacy_install_dir();

    // 新旧路径相同时无需迁移（exe 本身就在文档文件夹中）
    if legacy_dir == new_dir {
        return new_dir;
    }

    // 新旧目录都存在 → 使用新目录，提示用户可清理旧目录
    if legacy_dir.exists() && new_dir.exists() {
        eprintln!(
            "新旧安装目录同时存在，使用新位置: {}\n\
             旧目录可手动删除: {}",
            new_dir.display(),
            legacy_dir.display()
        );
        return new_dir;
    }

    // 旧目录存在且新目录不存在 → 迁移
    if legacy_dir.exists() && !new_dir.exists() {
        eprintln!(
            "检测到旧版安装，正在迁移: {} → {}",
            legacy_dir.display(),
            new_dir.display()
        );

        // 确保新目录的父目录存在
        if let Some(parent) = new_dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // 尝试 rename（同盘符下是原子操作，速度极快）
        match std::fs::rename(&legacy_dir, &new_dir) {
            Ok(()) => {
                eprintln!("迁移成功");
            }
            Err(e) => {
                // rename 失败（跨盘符等），回退到使用旧目录
                eprintln!(
                    "迁移失败（将继续使用旧位置）: {e}\n\
                     旧位置: {}\n新位置: {}",
                    legacy_dir.display(),
                    new_dir.display()
                );
                return legacy_dir;
            }
        }
    }

    new_dir
}
