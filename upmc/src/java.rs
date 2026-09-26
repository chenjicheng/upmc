//! Checks shared by the two managed Java installers.
use anyhow::{Context, Result, ensure};
use std::fs::File;
use std::path::Path;

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
