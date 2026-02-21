// ============================================================
// logging.rs — 文件日志模块
// ============================================================
// 将日志写入系统临时目录下的文件，替代内存日志。
// 发生错误时，GUI 直接读取日志文件内容展示给用户。
//
// 日志文件路径: %TEMP%/upmc-updater.log
// ============================================================

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

/// 全局日志文件路径（初始化后不可变）
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// 初始化日志文件。
///
/// 在系统临时目录创建（或清空）日志文件。
/// 应在程序启动时调用一次。
pub fn init() {
    let path = std::env::temp_dir().join("upmc-updater.log");
    // 清空旧日志
    let _ = fs::write(&path, "");
    LOG_PATH.set(path).ok();
}

/// 获取日志文件路径。
pub fn path() -> Option<&'static PathBuf> {
    LOG_PATH.get()
}

/// 向日志文件追加一行。
pub fn write(msg: impl std::fmt::Display) {
    if let Some(path) = LOG_PATH.get() {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(format!("{}\r\n", msg).as_bytes());
        }
    }
}

/// 读取完整日志内容。
pub fn read_all() -> String {
    match LOG_PATH.get() {
        Some(path) => fs::read_to_string(path).unwrap_or_default(),
        None => String::new(),
    }
}
