// ============================================================
// packwiz.rs — packwiz-installer 调用模块
// ============================================================
// 负责调用 packwiz-installer-bootstrap.jar，
// 让它根据远程 pack.toml 索引增量同步模组和配置文件。
//
// packwiz-installer-bootstrap 的工作原理：
//   1. 从指定 URL 下载 pack.toml 和 index.toml
//   2. 对比本地 .minecraft/ 中的文件
//   3. 下载新增/更新的文件，删除已移除的文件
//   4. 全程自动，无需用户交互
// ============================================================

use anyhow::{bail, Context, Result};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use crate::config;
use crate::retry;

/// 调用 packwiz-installer-bootstrap 同步模组和配置。
///
/// 等效于命令：
/// ```
/// java -jar packwiz-installer-bootstrap.jar \
///     -g              (无 GUI，静默运行)
///     -s client       (客户端模式)
///     https://xxx.github.io/upmc-dist/pack.toml
/// ```
///
/// `-g` 让 packwiz-installer 不弹出自己的窗口（我们有自己的 GUI）
/// `-s client` 指定只同步客户端需要的文件
///
/// 内置重试机制：如果同步失败（通常因网络不稳定），
/// 会自动重试最多 RETRY_MAX_ATTEMPTS 次。
pub fn sync_modpack(base_dir: &Path, pack_url: &str) -> Result<()> {
    let base_owned = base_dir.to_path_buf();
    let url_owned = pack_url.to_string();

    retry::with_retry(
        config::RETRY_MAX_ATTEMPTS,
        config::RETRY_BASE_DELAY_SECS,
        "模组同步",
        || sync_modpack_inner(&base_owned, &url_owned),
    )
}

/// sync_modpack 的内部实现（单次尝试）。
fn sync_modpack_inner(base_dir: &Path, pack_url: &str) -> Result<()> {
    let java = config::find_java(base_dir)?;
    let bootstrap_jar = base_dir.join(config::PACKWIZ_BOOTSTRAP_JAR);
    let mc_dir = base_dir.join(config::MINECRAFT_DIR);

    // 检查必要文件
    if !bootstrap_jar.exists() {
        bail!(
            "找不到 packwiz-installer-bootstrap: {}",
            bootstrap_jar.display()
        );
    }

    // 确保 .minecraft 目录存在
    std::fs::create_dir_all(&mc_dir).context("创建 .minecraft 目录失败")?;

    // 调用 packwiz-installer-bootstrap
    // 注意：工作目录设置为 .minecraft，
    // 因为 packwiz-installer 相对于工作目录来存放文件
    let output = Command::new(&java)
        .arg("-jar")
        .arg(&bootstrap_jar)
        .arg("-g") // 无头模式（不弹 GUI）
        .arg("-s")
        .arg("client") // 客户端模式
        .arg(pack_url) // 远程 pack.toml URL
        .current_dir(&mc_dir) // 工作目录 = .minecraft
        .creation_flags(config::CREATE_NO_WINDOW)
        .output()
        .context("启动 packwiz-installer 失败，请检查 Java 运行时是否正常")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);

        let exit_code_str = match output.status.code() {
            Some(code) => format!("{}", code),
            None => "未知（进程被终止）".to_string(),
        };

        // 分析输出，推断可能的失败原因
        let hints = diagnose_sync_failure(&stdout, &stderr);

        let stdout_display = if stdout.trim().is_empty() {
            "（无输出）".to_string()
        } else {
            stdout.trim().to_string()
        };
        let stderr_display = if stderr.trim().is_empty() {
            "（无输出）".to_string()
        } else {
            stderr.trim().to_string()
        };

        bail!(
            "模组同步失败（退出码: {}）\n\
             \n\
             ── 标准输出 ──\n{}\n\
             \n\
             ── 错误输出 ──\n{}\n\
             {}\n\
             建议: 请检查网络连接后重试，如果问题持续请截图联系管理员。",
            exit_code_str,
            stdout_display,
            stderr_display,
            hints,
        );
    }

    Ok(())
}

/// 分析 packwiz-installer 的输出，推断可能的失败原因。
fn diagnose_sync_failure(stdout: &str, stderr: &str) -> String {
    let combined = format!("{}\n{}", stdout.to_lowercase(), stderr.to_lowercase());
    let mut hints = Vec::new();

    if combined.contains("connection")
        || combined.contains("timeout")
        || combined.contains("timed out")
        || combined.contains("unresolvedaddressexception")
        || combined.contains("unknownhostexception")
        || combined.contains("connect")
        || combined.contains("网络")
    {
        hints.push("网络连接失败或超时，请检查网络是否正常");
    }

    if combined.contains("null") || combined.contains("nullpointerexception") {
        hints.push("版本信息获取失败（显示为 null），可能是网络问题导致远程数据未正确下载");
    }

    if hints.is_empty() && (combined.contains("java.lang") || combined.contains("exception")) {
        hints.push("packwiz-installer 运行时发生 Java 异常");
    }

    if combined.contains("ssl")
        || combined.contains("sslexception")
        || combined.contains("sslhandshakeexception")
        || combined.contains("handshake_failure")
        || combined.contains("certificate")
        || combined.contains("certificateerror")
    {
        hints.push("SSL/TLS 连接问题，可能是证书错误或网络代理干扰");
    }

    if combined.contains("permission") || combined.contains("access denied") {
        hints.push("文件访问权限不足，请检查目录权限或关闭占用文件的程序");
    }

    if hints.is_empty() {
        return String::new();
    }

    let hint_lines: Vec<String> = hints.iter().map(|h| format!("  • {}", h)).collect();
    format!("\n可能的原因:\n{}\n", hint_lines.join("\n"))
}
