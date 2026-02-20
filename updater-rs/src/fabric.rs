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
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::config;

/// Windows: 不创建控制台窗口
const CREATE_NO_WINDOW: u32 = 0x08000000;

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
    let java = config::find_java(base_dir)?;
    let installer_jar = base_dir.join(config::FABRIC_INSTALLER_JAR);
    let mc_dir = base_dir.join(config::MINECRAFT_DIR);

    // 检查必要文件是否存在
    if !installer_jar.exists() {
        bail!("找不到 Fabric 安装器: {}", installer_jar.display());
    }

    // 确保 .minecraft 目录存在
    fs::create_dir_all(&mc_dir).context("创建 .minecraft 目录失败")?;

    // Fabric 安装器在非 -noprofile 模式下需要 launcher_profiles.json 存在
    let profiles_json = mc_dir.join("launcher_profiles.json");
    if !profiles_json.exists() {
        fs::write(&profiles_json, r#"{"profiles":{}}"#)
            .context("创建 launcher_profiles.json 失败")?;
    }

    // 先确保原版 MC 客户端已下载
    // Fabric 安装器不会下载原版，PCL2 需要原版作为前置
    download_vanilla_version(&mc_dir, mc_version)?;

    // 调用 Fabric Installer（使用 -noprofile，PCL2 不需要）
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
        .creation_flags(CREATE_NO_WINDOW)
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

// ────────────────────────────────────────────────────────────
// 原版 MC 下载
// ────────────────────────────────────────────────────────────

/// Mojang 版本清单 API
const VERSION_MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

/// 确保原版 MC 客户端已下载（公开接口，供 update.rs 每次启动调用）。
/// 如果文件已存在会立即返回。
pub fn ensure_vanilla_client(base_dir: &Path, mc_version: &str) -> Result<()> {
    let mc_dir = base_dir.join(config::MINECRAFT_DIR);
    download_vanilla_version(&mc_dir, mc_version)
}

/// 修正 PCL2 的版本级别隔离设置。
///
/// PCL2 在首次检测到 Fabric 版本时会在
/// `versions/<version_tag>/PCL/Setup.ini` 中写入 `VersionArgumentIndieV2:True`，
/// 这会导致游戏目录被隔离到该版本文件夹下，而 packwiz 安装模组到 `.minecraft/mods/`，
/// 两者不一致导致游戏无法加载模组。
///
/// 本函数每次启动时调用，确保 `VersionArgumentIndieV2` 为 `False`。
pub fn fix_version_isolation(base_dir: &Path, version_tag: &str) -> Result<()> {
    let mc_dir = base_dir.join(config::MINECRAFT_DIR);
    let pcl_dir = mc_dir.join("versions").join(version_tag).join("PCL");
    let setup_ini = pcl_dir.join("Setup.ini");

    if setup_ini.exists() {
        // 读取现有文件并替换隔离设置
        let content = fs::read_to_string(&setup_ini)
            .context("读取版本级 Setup.ini 失败")?;

        if content.contains("VersionArgumentIndieV2:True") {
            let new_content = content.replace(
                "VersionArgumentIndieV2:True",
                "VersionArgumentIndieV2:False",
            );
            fs::write(&setup_ini, &new_content)
                .context("写入版本级 Setup.ini 失败")?;
        } else if !content.contains("VersionArgumentIndieV2:") {
            // 文件存在但没有这个 key，追加
            let mut new_content = content;
            if !new_content.ends_with('\n') {
                new_content.push('\n');
            }
            new_content.push_str("VersionArgumentIndieV2:False\n");
            fs::write(&setup_ini, &new_content)
                .context("写入版本级 Setup.ini 失败")?;
        }
        // 如果已经是 False 就不用改
    } else {
        // 文件还不存在（Fabric 安装后但 PCL2 还没运行过），提前创建
        fs::create_dir_all(&pcl_dir)
            .context("创建版本级 PCL 目录失败")?;
        fs::write(&setup_ini, "VersionArgumentIndieV2:False\n")
            .context("写入版本级 Setup.ini 失败")?;
    }

    Ok(())
}

/// 下载原版 MC 客户端的 version JSON 和 client.jar。
///
/// Fabric 安装器不下载原版客户端，只安装 loader。
/// PCL2 需要原版 MC 作为前置版本才能启动 Fabric。
///
/// 流程：
///   1. 从 Mojang API 获取版本清单
///   2. 找到对应版本的 JSON URL
///   3. 下载 version JSON → versions/<ver>/<ver>.json
///   4. 从 JSON 中提取 client jar URL
///   5. 下载 client.jar → versions/<ver>/<ver>.jar
fn download_vanilla_version(mc_dir: &Path, mc_version: &str) -> Result<()> {
    let ver_dir = mc_dir.join("versions").join(mc_version);
    let ver_json_path = ver_dir.join(format!("{}.json", mc_version));
    let ver_jar_path = ver_dir.join(format!("{}.jar", mc_version));

    // 如果已经存在就跳过
    if ver_json_path.exists() && ver_jar_path.exists() {
        return Ok(());
    }

    fs::create_dir_all(&ver_dir)
        .with_context(|| format!("创建版本目录失败: {}", ver_dir.display()))?;

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(config::DOWNLOAD_TIMEOUT_SECS))
        .build();

    // 1. 获取版本清单
    let manifest_str = agent
        .get(VERSION_MANIFEST_URL)
        .call()
        .context("获取 Mojang 版本清单失败")?
        .into_string()
        .context("读取版本清单失败")?;

    let manifest: serde_json::Value = serde_json::from_str(&manifest_str)
        .context("解析版本清单 JSON 失败")?;

    // 2. 找到目标版本的 URL
    let versions = manifest["versions"]
        .as_array()
        .context("版本清单格式错误")?;

    let version_url = versions
        .iter()
        .find(|v| v["id"].as_str() == Some(mc_version))
        .and_then(|v| v["url"].as_str())
        .with_context(|| format!("在 Mojang 清单中找不到版本 {}", mc_version))?
        .to_string();

    // 3. 下载 version JSON
    if !ver_json_path.exists() {
        let ver_json_str = agent
            .get(&version_url)
            .call()
            .with_context(|| format!("下载 MC {} version JSON 失败", mc_version))?
            .into_string()
            .context("读取 version JSON 失败")?;

        fs::write(&ver_json_path, &ver_json_str)
            .with_context(|| format!("写入 {} 失败", ver_json_path.display()))?;
    }

    // 4. 从 version JSON 中提取 client jar URL 并下载
    if !ver_jar_path.exists() {
        let ver_json_str = fs::read_to_string(&ver_json_path)
            .context("读取 version JSON 失败")?;
        let ver_json: serde_json::Value = serde_json::from_str(&ver_json_str)
            .context("解析 version JSON 失败")?;

        let client_url = ver_json["downloads"]["client"]["url"]
            .as_str()
            .context("version JSON 中找不到客户端下载地址")?;

        // 下载 client.jar（约 20-30 MB）
        let response = agent
            .get(client_url)
            .call()
            .with_context(|| format!("下载 MC {} 客户端 jar 失败", mc_version))?;

        let mut reader = response.into_reader();
        let mut file = fs::File::create(&ver_jar_path)
            .with_context(|| format!("创建 {} 失败", ver_jar_path.display()))?;

        let mut buf = [0u8; 65536];
        loop {
            let n = reader.read(&mut buf).context("读取客户端 jar 数据失败")?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).context("写入客户端 jar 失败")?;
        }
    }

    Ok(())
}
