// ============================================================
// fabric.rs — Fabric 安装器调用模块
// ============================================================
// 负责：
//   1. 调用 fabric-installer.jar 的 CLI 模式安装指定版本
//   2. 为首次安装创建默认启动配置
// 玩家已有版本目录和配置由玩家保留，更新不执行目录清理。
// ============================================================

use anyhow::{Context, Result, bail};
use std::fs;
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use crate::config;
use crate::retry;

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn fabric_path_non_ansi_root_installs_into_existing_game_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::java_test_fixture::install_root(dir.path());
        fs::write(
            root.join(config::FABRIC_INSTALLER_JAR),
            crate::java_test_fixture::jar_bytes(),
        )
        .unwrap();
        install_fabric(&root, "1.21.11", "0.19.5").unwrap();
        assert_eq!(
            fs::read(root.join(".minecraft/fabric-path-probe.txt")).unwrap(),
            b"UPMC_JAR_PATH_OK"
        );
        assert!(!root.join("fabric-path-probe.txt").exists());
    }

    #[test]
    fn fabric_path_locked_jar_is_diagnosed_before_writing_game_directory() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = crate::java_test_fixture::install_root(dir.path());
        let jar = root.join(config::FABRIC_INSTALLER_JAR);
        fs::write(&jar, b"PKfixture").unwrap();
        let _locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&jar)
            .unwrap();
        let error = install_fabric(&root, "1.21.11", "0.19.5").unwrap_err();
        assert!(
            !root.join(".minecraft/launcher_profiles.json").exists(),
            "local JAR failure must precede game/network work: {error:#}"
        );
        assert!(format!("{error:#}").contains("安装器"));
    }

    #[test]
    fn fabric_path_invalid_jar_is_rejected_before_writing_profiles() {
        for contents in [None, Some(&b""[..]), Some(&b"PKnot a complete archive"[..])] {
            let dir = tempfile::tempdir().unwrap();
            let root = crate::java_test_fixture::install_root(dir.path());
            let jar = root.join(config::FABRIC_INSTALLER_JAR);
            if let Some(bytes) = contents {
                fs::write(&jar, bytes).unwrap();
            } else {
                fs::create_dir(&jar).unwrap();
            }
            let error = install_fabric(&root, "1.21.11", "0.19.5").unwrap_err();
            assert!(
                !root.join(".minecraft/launcher_profiles.json").exists(),
                "invalid JAR must precede game writes: {error:#}"
            );
            assert!(format!("{error:#}").contains("安装器"));
        }
    }
}

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
pub fn install_fabric(base_dir: &Path, mc_version: &str, fabric_version: &str) -> Result<()> {
    let java = config::find_java_in(base_dir)?;
    let installer_jar = base_dir.join(config::FABRIC_INSTALLER_JAR);
    let mc_dir = base_dir.join(config::MINECRAFT_DIR);

    let version_tag = format!("fabric-loader-{fabric_version}-{mc_version}");
    let installed = mc_dir.join("versions").join(&version_tag).join(format!("{version_tag}.json"));
    if installed.exists() {
        // Missing updater metadata must not reinstall over an existing profile.
        return Ok(());
    }

    crate::java::validate_installer_jar(&installer_jar, "Fabric")?;

    // 确保 .minecraft 目录存在
    fs::create_dir_all(&mc_dir).context("创建 .minecraft 目录失败")?;

    // Fabric 安装器在非 -noprofile 模式下需要 launcher_profiles.json 存在
    let profiles_json = mc_dir.join("launcher_profiles.json");
    if !profiles_json.exists() {
        crate::managed_mods::create_default(&mc_dir, Path::new("launcher_profiles.json"), br#"{"profiles":{}}"#)?;
    }

    // 前置验证 Java 可用
    verify_java(&java)?;

    // 先确保原版 MC 客户端已下载
    // Fabric 安装器不会下载原版，PCL2 需要原版作为前置
    download_vanilla_version(&mc_dir, mc_version)?;

    // 调用 Fabric Installer（使用 -noprofile，PCL2 不需要）
    // 使用 BMCLAPI 镜像加速国内下载
    let output = Command::new(&java)
        .env_remove("JAVA_TOOL_OPTIONS")
        .env_remove("JDK_JAVA_OPTIONS")
        .env_remove("_JAVA_OPTIONS")
        .arg("-jar")
        // Java's native launcher can lose characters outside the Windows ANSI
        // codepage. CreateProcessW preserves the Unicode working directory.
        .arg(config::FABRIC_INSTALLER_JAR)
        .arg("client")
        .arg("-dir")
        .arg(config::MINECRAFT_DIR)
        .arg("-mcversion")
        .arg(mc_version)
        .arg("-loader")
        .arg(fabric_version)
        .arg("-noprofile")
        .arg("-metaurl")
        .arg(config::FABRIC_META_URL)
        .arg("-mavenurl")
        .arg(config::FABRIC_MAVEN_URL)
        .current_dir(base_dir)
        .creation_flags(config::CREATE_NO_WINDOW)
        .output()
        .with_context(|| {
            format!(
                "启动 Fabric 安装器失败\nJava: {}\n安装器: {}\n工作目录: {}",
                java.display(),
                installer_jar.display(),
                base_dir.display()
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let advice = crate::java::local_failure_hint(&format!("{stdout}\n{stderr}")).unwrap_or(
            "请根据以上输出检查 Java 运行环境和下载连接；问题持续时请提供完整错误信息。",
        );

        let exit_code_str = match output.status.code() {
            Some(code) => format!("{}", code),
            None => "未知（进程被终止）".to_string(),
        };

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
            "Fabric 安装失败（退出码: {}）\n\
             \n\
             ── 标准输出 ──\n{}\n\
             \n\
             ── 错误输出 ──\n{}\n\
             \n\
             目标版本: MC {} + Fabric Loader {}\n\
             Java: {}\n安装器: {}\n工作目录: {}\n建议: {}",
            exit_code_str,
            stdout_display,
            stderr_display,
            mc_version,
            fabric_version,
            java.display(),
            installer_jar.display(),
            base_dir.display(),
            advice,
        );
    }

    Ok(())
}

/// 前置验证 Java 是否能正常启动。
///
/// 运行 `java -version`，如果失败则自动打开下载页面并返回错误。
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

/// Initialize defaults during the first install only; never edit an existing
/// PCL setup file or override a player's isolation preference.
pub fn initialize_version_isolation(base_dir: &Path, version_tag: &str) -> Result<()> {
    let relative = Path::new("versions").join(version_tag).join("PCL/Setup.ini");
    crate::managed_mods::create_default(
        &base_dir.join(config::MINECRAFT_DIR),
        &relative,
        b"VersionArgumentIndieV2:False\n",
    )
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
    let mc_dir_owned = mc_dir.to_path_buf();
    let ver_owned = mc_version.to_string();

    retry::with_retry(
        config::RETRY_MAX_ATTEMPTS,
        config::RETRY_BASE_DELAY_SECS,
        &format!("下载原版 MC {}", mc_version),
        || download_vanilla_version_inner(&mc_dir_owned, &ver_owned),
    )
}

/// download_vanilla_version 的内部实现（单次尝试）。
fn download_vanilla_version_inner(mc_dir: &Path, mc_version: &str) -> Result<()> {
    let ver_dir = mc_dir.join("versions").join(mc_version);
    let ver_json_path = ver_dir.join(format!("{mc_version}.json"));
    let ver_jar_path = ver_dir.join(format!("{mc_version}.jar"));

    // 如果已经存在就跳过
    if ver_json_path.exists() && ver_jar_path.exists() {
        return Ok(());
    }

    fs::create_dir_all(&ver_dir)
        .with_context(|| format!("创建版本目录失败: {}", ver_dir.display()))?;

    let agent = config::download_agent();

    // 1. 获取版本清单
    let manifest_str = agent
        .get(VERSION_MANIFEST_URL)
        .call()
        .context("获取 Mojang 版本清单失败")?
        .body_mut()
        .read_to_string()
        .context("读取版本清单失败")?;

    let manifest: serde_json::Value =
        serde_json::from_str(&manifest_str).context("解析版本清单 JSON 失败")?;

    // 2. 找到目标版本的 URL
    let versions = manifest["versions"]
        .as_array()
        .context("版本清单格式错误")?;

    let version_url = versions
        .iter()
        .find(|v| v["id"].as_str() == Some(mc_version))
        .and_then(|v| v["url"].as_str())
        .with_context(|| format!("在 Mojang 清单中找不到版本 {mc_version}"))?
        .to_string();

    // 3. 下载 version JSON
    if !ver_json_path.exists() {
        let ver_json_str = agent
            .get(&version_url)
            .call()
            .with_context(|| format!("下载 MC {mc_version} version JSON 失败"))?
            .body_mut()
            .read_to_string()
            .context("读取 version JSON 失败")?;

        fs::write(&ver_json_path, &ver_json_str)
            .with_context(|| format!("写入 {} 失败", ver_json_path.display()))?;
    }

    // 4. 从 version JSON 中提取 client jar URL 并下载
    if !ver_jar_path.exists() {
        let ver_json_str = fs::read_to_string(&ver_json_path).context("读取 version JSON 失败")?;
        let ver_json: serde_json::Value =
            serde_json::from_str(&ver_json_str).context("解析 version JSON 失败")?;

        let client_url = ver_json["downloads"]["client"]["url"]
            .as_str()
            .context("version JSON 中找不到客户端下载地址")?;

        // 下载 client.jar（约 20-30 MB）
        let response = agent
            .get(client_url)
            .call()
            .with_context(|| format!("下载 MC {mc_version} 客户端 jar 失败"))?;

        let mut reader = response.into_body().into_reader();
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
