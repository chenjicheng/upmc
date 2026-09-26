//! Checks shared by the two managed Java installers.
use anyhow::{Context, Result, ensure};
use std::fs::{self, File};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use crate::{bootstrap, config, update::Progress, version::Downloads};

const DEFAULT_JRE_URL: &str = "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.6%2B7/OpenJDK21U-jre_x64_windows_hotspot_21.0.6_7.zip";
const DEFAULT_JRE_SHA256: &str = "707c981a4ff9e680a9ea5d6f625eafe8bc47e1f89140a67d761fde24fc02ab49";

fn runtime_works(root: &Path) -> bool {
    if !root.join("bin/javaw.exe").is_file() {
        return false;
    }
    let Ok(output) = Command::new(root.join("bin/java.exe"))
        .env_remove("JAVA_TOOL_OPTIONS")
        .env_remove("JDK_JAVA_OPTIONS")
        .env_remove("_JAVA_OPTIONS")
        .arg("-version")
        .creation_flags(config::CREATE_NO_WINDOW)
        .output()
    else {
        return false;
    };
    output.status.success()
        && String::from_utf8_lossy(&output.stderr)
            .lines()
            .any(|line| line.contains("version \"21.") || line.contains("version \"21\""))
}

/// Keep an independent Java 21 inside this modpack, regardless of system Java.
pub(crate) fn ensure_runtime(
    base_dir: &Path,
    downloads: &Downloads,
    on_progress: &dyn Fn(Progress),
) -> Result<()> {
    let runtime = std::path::absolute(base_dir.join(config::MANAGED_JAVA_DIR))?;
    if runtime_works(&runtime) {
        on_progress(Progress::new(54, "整合包 Java 21 已就绪"));
        return Ok(());
    }
    let parent = runtime.parent().context("Java 目录无父目录")?;
    fs::create_dir_all(parent)?;
    let lock = fs::OpenOptions::new()
        .create(true).truncate(false).read(true).write(true)
        .open(parent.join("java-install.lock"))?;
    fs2::FileExt::lock_exclusive(&lock).context("等待 Java 安装锁失败")?;
    if runtime_works(&runtime) {
        return Ok(());
    }

    let url = downloads.jre_url.as_deref().unwrap_or(DEFAULT_JRE_URL);
    let default_url = config::github_proxy_url(DEFAULT_JRE_URL);
    let sha256 = downloads.jre_sha256.as_deref().filter(|s| !s.is_empty())
        .or_else(|| (config::github_proxy_url(url) == default_url).then_some(DEFAULT_JRE_SHA256))
        .context("自定义 Java 下载地址缺少 SHA256，请管理员补充 downloads.jre_sha256")?;
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
    let stage = parent.join(format!("java-stage-{}-{nonce}", std::process::id()));
    fs::create_dir(&stage)?;
    let result = (|| -> Result<()> {
        on_progress(Progress::new(50, "正在自动下载整合包 Java 21..."));
        let archive = stage.join("java.zip");
        bootstrap::download_file_verified(url, &archive, sha256, on_progress, 50, 53)?;
        let unpacked = stage.join("unpacked");
        bootstrap::extract_zip(&archive, &unpacked).context("解压 Java 21 失败")?;
        let mut extracted = unpacked.clone();
        if !extracted.join("bin/java.exe").is_file() {
            extracted = fs::read_dir(&unpacked)?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .find(|path| path.join("bin/java.exe").is_file())
                .context("Java 压缩包中没有 bin/java.exe")?;
        }
        ensure!(runtime_works(&extracted), "下载的 Java 21 无法启动，请检查安全软件是否拦截。");
        let backup = parent.join(format!("java-backup-{}-{nonce}", std::process::id()));
        let had_runtime = runtime.exists();
        if had_runtime {
            fs::rename(&runtime, &backup).context("无法替换旧 Java，请关闭游戏后重试")?;
        }
        if let Err(error) = fs::rename(&extracted, &runtime) {
            if had_runtime {
                let _ = fs::rename(&backup, &runtime);
            }
            return Err(error).context("安装整合包 Java 21 失败");
        }
        if had_runtime {
            let _ = fs::remove_dir_all(&backup);
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&stage);
    result?;
    on_progress(Progress::new(54, "整合包 Java 21 安装完成"));
    Ok(())
}

pub(crate) fn configure_launcher(command: &mut Command, base_dir: &Path) -> Result<()> {
    let runtime = std::path::absolute(base_dir.join(config::MANAGED_JAVA_DIR))?;
    if runtime.join("bin/java.exe").is_file() {
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let paths = std::iter::once(runtime.join("bin")).chain(std::env::split_paths(&old_path));
        command.env("JAVA_HOME", &runtime)
            .env("PATH", std::env::join_paths(paths)?)
            .env_remove("JAVA_TOOL_OPTIONS")
            .env_remove("JDK_JAVA_OPTIONS")
            .env_remove("_JAVA_OPTIONS");
    }
    Ok(())
}

/// Check with native Windows paths before the installer can write game files.
/// Download authenticity remains enforced by bootstrap's existing SHA256 checks.
pub(crate) fn validate_installer_jar(path: &Path, name: &str) -> Result<()> {
    let file = File::open(path).with_context(|| {
        format!(
            "无法读取 {name} 安装器文件: {}\n请检查文件是否存在、访问权限或是否被其他程序占用。",
            path.display()
        )
    })?;
    ensure!(
        file.metadata()?.is_file(),
        "{name} 安装器路径不是普通文件: {}",
        path.display()
    );
    let mut archive = zip::ZipArchive::new(file).with_context(|| {
        format!(
            "{name} 安装器文件损坏或不完整: {}\n请重新下载该安装器。",
            path.display()
        )
    })?;
    archive.by_name("META-INF/MANIFEST.MF").with_context(|| {
        format!(
            "{name} 安装器缺少 JAR 清单: {}\n请重新下载该安装器。",
            path.display()
        )
    })?;
    Ok(())
}

pub(crate) fn local_failure_hint(output: &str) -> Option<&'static str> {
    let output = output.to_ascii_lowercase();
    if output.contains("unable to access jarfile") {
        Some(
            "Java 无法读取本地安装器文件。请检查下方 Java 路径、安装器路径、文件访问权限，以及安全软件的拦截记录。",
        )
    } else if output.contains("invalid or corrupt jarfile") {
        Some("本地安装器文件损坏或不是有效的 JAR，请重新下载该安装器。")
    } else if output.contains("error decoding percent encoded characters") {
        Some(
            "Java 无法解析安装路径中的特殊 Unicode 字符（例如 emoji）。请保留当前目录并将完整错误信息反馈给管理员。",
        )
    } else {
        None
    }
}
