// ============================================================
// retry.rs — 网络操作重试模块
// ============================================================
// 为需要联网的操作提供自动重试机制。
// 使用指数退避策略：每次失败后等待时间翻倍。
//
// 用法示例：
//   retry::with_retry(3, 3, "下载文件", || {
//       download_something()
//   })
// ============================================================

use std::thread;
use std::time::Duration;

use anyhow::Result;

/// 执行一个操作，失败时自动重试。
///
/// # 参数
/// - `max_attempts`: 最大尝试次数（含首次）
/// - `base_delay_secs`: 首次重试前等待秒数，后续翻倍（指数退避）
/// - `operation_name`: 操作名称（用于日志）
/// - `f`: 要执行的操作闭包
///
/// # 退避策略
/// 等待时间 = base_delay_secs × 2^(attempt-1)
/// 例如 base_delay_secs=3: 首次失败后等 3s，第二次等 6s
///
/// # 返回
/// 第一次成功的结果，或最后一次失败的错误（附加重试信息）
pub fn with_retry<F, T>(
    max_attempts: u32,
    base_delay_secs: u64,
    operation_name: &str,
    f: F,
) -> Result<T>
where
    F: Fn() -> Result<T>,
{
    let mut last_error = None;

    for attempt in 1..=max_attempts {
        match f() {
            Ok(value) => {
                if attempt > 1 {
                    eprintln!(
                        "[重试] {} 在第 {} 次尝试时成功",
                        operation_name, attempt
                    );
                }
                return Ok(value);
            }
            Err(e) => {
                if attempt < max_attempts {
                    let delay = base_delay_secs.saturating_mul(2u64.saturating_pow(attempt - 1));
                    eprintln!(
                        "[重试] {} 失败（第 {}/{} 次尝试），{} 秒后重试...\n  原因: {:#}",
                        operation_name, attempt, max_attempts, delay, e
                    );
                    thread::sleep(Duration::from_secs(delay));
                } else {
                    eprintln!(
                        "[重试] {} 在 {} 次尝试后仍然失败",
                        operation_name, max_attempts
                    );
                }
                last_error = Some(e);
            }
        }
    }

    // Safe unwrap: loop guarantees at least one attempt was made
    let err = last_error.unwrap();
    Err(err.context(format!(
        "{} 重试 {} 次后仍然失败",
        operation_name, max_attempts
    )))
}
