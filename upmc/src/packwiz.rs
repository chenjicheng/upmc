// ============================================================
// packwiz.rs — packwiz-installer 调用模块
// ============================================================
// 负责调用 packwiz-installer-bootstrap.jar，
// 在更新器独立目录中下载，再只同步已记录的模组；保留玩家配置。
//
// packwiz-installer-bootstrap 的工作原理：
//   1. 从指定 URL 下载 pack.toml 和 index.toml
//   2. 对比本地 .minecraft/ 中的文件
//   3. 下载新增/更新的文件，删除已移除的文件
//   4. 全程自动，无需用户交互
// ============================================================

use anyhow::{Context, Result, bail};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use crate::config;
use crate::retry;

// Pin the actual installer as well as its bootstrap. Java's default updater
// otherwise contacts GitHub directly and bypasses the configured proxy.
const INSTALLER_URL: &str =
    "https://github.com/packwiz/packwiz-installer/releases/download/v0.5.14/packwiz-installer.jar";
const INSTALLER_SHA256: &str = "c9f646908d340d84773948a9a7d98bc1dae250d35e1016dc6e2b8459760b5598";

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
pub fn sync_modpack(base_dir: &Path, pack_url: &str, first_install: bool) -> Result<()> {
    // ── 前置检查（确定性失败，不需要重试） ──
    let java = config::find_java_in(base_dir)?;
    let bootstrap_jar = base_dir.join(config::PACKWIZ_BOOTSTRAP_JAR);
    let workspace = crate::managed_mods::prepare_workspace(base_dir)?;

    crate::java::validate_installer_jar(&bootstrap_jar, "Packwiz")?;

    let installer =
        crate::managed_mods::safe_path(base_dir, Path::new("updater/packwiz-installer.jar"))?;
    if crate::bootstrap::verify_sha256(&installer, INSTALLER_SHA256).is_err() {
        crate::bootstrap::download_file_verified(
            INSTALLER_URL,
            &installer,
            INSTALLER_SHA256,
            &|_| {},
            80,
            80,
        )?;
    }

    // 前置验证 Java 可用（确定性失败，不进入重试循环）
    verify_java(&java)?;

    // ── 网络操作（可能因网络波动失败，需要重试） ──
    let url_owned = config::github_proxy_url(pack_url);

    retry::with_retry(
        config::RETRY_MAX_ATTEMPTS,
        config::RETRY_BASE_DELAY_SECS,
        "模组同步",
        || run_packwiz_installer(&java, &bootstrap_jar, &workspace, &url_owned),
    )?;
    crate::managed_mods::apply(base_dir, first_install)
}

/// 执行 packwiz-installer 进程（单次尝试）。
fn run_packwiz_installer(
    java: &Path,
    bootstrap_jar: &Path,
    workspace: &Path,
    pack_url: &str,
) -> Result<()> {
    // 调用 packwiz-installer-bootstrap
    // Packwiz may replace/remove files from its index. Its entire working
    // directory is updater-owned, never the player's .minecraft directory.
    let output = Command::new(java)
        .env_remove("JAVA_TOOL_OPTIONS")
        .env_remove("JDK_JAVA_OPTIONS")
        .env_remove("_JAVA_OPTIONS")
        .arg("-jar")
        // ASCII relative JAR path preserves the 0.5.7 native path fix.
        .arg(
            Path::new("..").join(
                bootstrap_jar
                    .file_name()
                    .context("Packwiz 安装器无文件名")?,
            ),
        )
        .arg("-g") // 无头模式（不弹 GUI）
        .args([
            "--bootstrap-no-update",
            "--bootstrap-main-jar",
            "../packwiz-installer.jar",
        ])
        .arg("-s")
        .arg("client") // 客户端模式
        .args(["--pack-folder", ".", "--multimc-folder", "."])
        .arg(pack_url) // 远程 pack.toml URL
        .current_dir(workspace)
        .creation_flags(config::CREATE_NO_WINDOW)
        .output()
        .with_context(|| {
            format!(
                "启动 packwiz-installer 失败\nJava: {}\n安装器: {}\n工作目录: {}",
                java.display(),
                bootstrap_jar.display(),
                workspace.display()
            )
        })?;

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
             Java: {}\n安装器: {}\n工作目录: {}\n\
             问题持续时请提供完整错误信息。",
            exit_code_str,
            stdout_display,
            stderr_display,
            hints,
            java.display(),
            bootstrap_jar.display(),
            workspace.display(),
        );
    }

    Ok(())
}

/// 前置验证 Java 是否能正常启动。
///
/// 运行 `java -version`，如果失败且输出包含环境异常关键词，
/// 自动打开下载页面并返回错误（不进入重试循环）。
fn verify_java(java: &Path) -> Result<()> {
    let output = Command::new(java)
        .env_remove("JAVA_TOOL_OPTIONS")
        .env_remove("JDK_JAVA_OPTIONS")
        .env_remove("_JAVA_OPTIONS")
        .arg("-version")
        .creation_flags(config::CREATE_NO_WINDOW)
        .output()
        .context("无法启动 Java 进程")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        let _ = Command::new("cmd")
            .args(["/c", "start", "", config::JAVA_DOWNLOAD_URL])
            .creation_flags(config::CREATE_NO_WINDOW)
            .spawn();

        bail!(
            "Java 环境异常\n\
             \n\
             错误信息: {}\n\
             \n\
             当前 Java 路径: {}\n\
             该 Java 安装可能已损坏或版本不兼容。\n\
             正在尝试打开 Java 下载页面，如未自动打开请手动访问：\n\
             {}\n\
             \n\
             安装完成后请重新运行更新器。",
            stderr.trim(),
            java.display(),
            config::JAVA_DOWNLOAD_URL,
        );
    }

    Ok(())
}

/// 分析 packwiz-installer 的输出，推断可能的失败原因。
fn diagnose_sync_failure(stdout: &str, stderr: &str) -> String {
    let combined = format!("{}\n{}", stdout.to_lowercase(), stderr.to_lowercase());
    if let Some(hint) = crate::java::local_failure_hint(&combined) {
        return format!("\n{hint}\n");
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packwiz_path_non_ansi_root_uses_separate_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::java_test_fixture::install_root(dir.path());
        let jar = root.join(config::PACKWIZ_BOOTSTRAP_JAR);
        std::fs::write(&jar, crate::java_test_fixture::jar_bytes()).unwrap();
        let workspace = crate::managed_mods::prepare_workspace(&root).unwrap();
        run_packwiz_installer(
            &config::find_java().unwrap(),
            &jar,
            &workspace,
            "https://example.invalid/pack.toml",
        )
        .unwrap();
        assert_eq!(
            std::fs::read(workspace.join("packwiz-path-probe.txt")).unwrap(),
            b"UPMC_JAR_PATH_OK"
        );
        assert!(!root.join("packwiz-path-probe.txt").exists());
        assert!(!root.join(".minecraft/packwiz-path-probe.txt").exists());
        let args = std::fs::read_to_string(workspace.join("installer-args.txt")).unwrap();
        assert!(
            args.contains("--bootstrap-no-update"),
            "Java must not download an unproxied installer: {args}"
        );
        assert!(args.contains("--bootstrap-main-jar\n../packwiz-installer.jar"));
    }

    #[test]
    fn packwiz_path_access_error_diagnoses_local_file() {
        let hint = diagnose_sync_failure(
            "",
            "Error: Unable to access jarfile ../updater/packwiz-installer-bootstrap.jar",
        );
        assert!(
            hint.contains("文件"),
            "missing actionable local-file diagnosis: {hint}"
        );
        assert!(!hint.contains("网络"));
    }

    #[test]
    fn diagnose_network_error() {
        let result = diagnose_sync_failure("", "java.net.UnknownHostException: example.com");
        assert!(result.contains("网络"));
    }

    #[test]
    fn diagnose_timeout() {
        let result = diagnose_sync_failure("Connection timed out", "");
        assert!(result.contains("网络"));
    }

    #[test]
    fn diagnose_ssl_error() {
        let result = diagnose_sync_failure("", "javax.net.ssl.SSLHandshakeException: ...");
        assert!(result.contains("SSL"));
    }

    #[test]
    fn diagnose_permission_error() {
        let result = diagnose_sync_failure("", "Access denied to file mods/test.jar");
        assert!(result.contains("权限"));
    }

    #[test]
    fn diagnose_null_pointer() {
        let result = diagnose_sync_failure("NullPointerException at ...", "");
        assert!(result.contains("null"));
    }

    #[test]
    fn diagnose_generic_java_exception() {
        let result = diagnose_sync_failure("", "java.lang.IllegalStateException: bad state");
        assert!(result.contains("Java 异常"));
    }

    #[test]
    fn diagnose_no_match() {
        let result = diagnose_sync_failure("all good", "no errors");
        assert!(result.is_empty());
    }
}
