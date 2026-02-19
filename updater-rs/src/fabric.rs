// ============================================================
// fabric.rs — Fabric 安装器调用模块
// ============================================================
// 负责：
//   1. 调用 fabric-installer.jar 的 CLI 模式安装指定版本
//   2. 清理旧的 versions/ 目录（只保留新版本）
//   3. 清空 mods/ 目录（packwiz 会重新同步正确的模组）
// ============================================================

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::config;

/// 调用 Fabric Installer CLI 安装指定版本的 MC + Fabric Loader。
///
/// 等效于命令：
/// ```
/// java -jar fabric-installer.jar client \
///     -dir ".minecraft" \
///     -mcversion 1.21.4 \
///     -loader 0.16.9 \
///     -noprofile
/// ```
///
/// `-noprofile` 表示不写入启动器 profile（由 PCL2 自己管理）。
pub fn install_fabric(
    base_dir: &Path,
    mc_version: &str,
    fabric_version: &str,
) -> Result<()> {
    let java = base_dir.join(config::JAVA_EXE);
    let installer_jar = base_dir.join(config::FABRIC_INSTALLER_JAR);
    let mc_dir = base_dir.join(config::MINECRAFT_DIR);

    // 检查必要文件是否存在
    if !java.exists() {
        bail!("找不到 Java: {}", java.display());
    }
    if !installer_jar.exists() {
        bail!("找不到 Fabric 安装器: {}", installer_jar.display());
    }

    // 确保 .minecraft 目录存在
    fs::create_dir_all(&mc_dir).context("创建 .minecraft 目录失败")?;

    // 调用 Fabric Installer
    let output = Command::new(&java)
        .arg("-jar")
        .arg(&installer_jar)
        .arg("client")
        .arg("-dir")
        .arg(&mc_dir)
        .arg("-mcversion")
        .arg(mc_version)
        .arg("-loader")
        .arg(fabric_version)
        .arg("-noprofile")
        .output()
        .context("启动 Fabric 安装器失败")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "Fabric 安装失败 (exit code: {:?}):\nstdout: {}\nstderr: {}",
            output.status.code(),
            stdout,
            stderr
        );
    }

    Ok(())
}

/// 清理旧的 versions/ 目录。
///
/// 扫描 .minecraft/versions/ 下的所有子目录，
/// 只保留 `keep_tag` 指定的版本文件夹，删除其余所有。
///
/// 这确保玩家的 .minecraft/versions/ 里不会堆积旧版本文件。
pub fn cleanup_old_versions(base_dir: &Path, keep_tag: &str) -> Result<()> {
    let versions_dir = base_dir.join(config::MINECRAFT_DIR).join("versions");

    if !versions_dir.exists() {
        // 首次安装，还没有 versions 目录，无需清理
        return Ok(());
    }

    let entries = fs::read_dir(&versions_dir).context("读取 versions 目录失败")?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // 只处理目录
        if !path.is_dir() {
            continue;
        }

        // 获取目录名
        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };

        // 保留新版本的目录，删除其他
        if dir_name != keep_tag {
            fs::remove_dir_all(&path)
                .with_context(|| format!("删除旧版本目录失败: {}", dir_name))?;
        }
    }

    Ok(())
}

/// 清空 mods/ 目录中的所有 .jar 文件。
///
/// 大版本升级时，旧模组可能不兼容新版本，
/// 所以先全部清空，然后由 packwiz 重新同步正确版本的模组。
///
/// 注意：只删除 .jar 文件，保留 packwiz-installer-bootstrap 不在此目录。
pub fn clean_mods_dir(base_dir: &Path) -> Result<()> {
    let mods_dir = base_dir.join(config::MINECRAFT_DIR).join("mods");

    if !mods_dir.exists() {
        return Ok(());
    }

    let entries = fs::read_dir(&mods_dir).context("读取 mods 目录失败")?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // 只删除 .jar 文件
        if path.is_file() {
            if let Some(ext) = path.extension() {
                if ext == "jar" {
                    fs::remove_file(&path)
                        .with_context(|| format!("删除模组失败: {}", path.display()))?;
                }
            }
        }
    }

    Ok(())
}
