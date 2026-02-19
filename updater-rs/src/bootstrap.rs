// ============================================================
// bootstrap.rs — 首次运行自举模块
// ============================================================
// 当玩家第一次双击 exe 时，.minecraft、PCL2、JRE 等都不存在。
// 此模块负责：
//   1. 检测哪些组件缺失
//   2. 从远程下载所有必要文件
//   3. 解压 JRE
//   4. 生成 PCL2 配置文件 (Setup.ini)
//   5. 创建目录结构
//
// 所有下载 URL 来自 server.json 的 downloads 字段，
// 管理员可远程控制下载源。
// ============================================================

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use crate::config;
use crate::update::Progress;
use crate::version::Downloads;

/// 检查是否需要首次安装（任一关键组件缺失）
pub fn needs_bootstrap(base_dir: &Path) -> bool {
    let checks = [
        config::PCL2_EXE,
        config::PACKWIZ_BOOTSTRAP_JAR,
        config::FABRIC_INSTALLER_JAR,
    ];

    checks.iter().any(|path| !base_dir.join(path).exists())
}

/// 检查是否已经完成过首次安装（PCL2 存在即可启动离线模式）
pub fn is_bootstrapped(base_dir: &Path) -> bool {
    base_dir.join(config::PCL2_EXE).exists()
}

/// 执行首次安装流程。
///
/// 根据 server.json 中的 downloads 字段，下载所有缺失的组件。
/// 通过 on_progress 回调报告进度（占总进度的 0%-50%）。
pub fn run_bootstrap(
    base_dir: &Path,
    downloads: &Downloads,
    on_progress: &dyn Fn(Progress),
) -> Result<()> {
    // ── 创建目录结构 ──
    on_progress(Progress::new(2, "正在创建目录结构..."));
    let dirs = [
        "PCL",
        ".minecraft",
        ".minecraft/mods",
        ".minecraft/config",
        "updater",
    ];
    for dir in &dirs {
        fs::create_dir_all(base_dir.join(dir))
            .with_context(|| format!("创建目录失败: {}", dir))?;
    }

    // ── 下载 JRE（如果不存在） ──
    let jre_java = base_dir.join("jre/bin/java.exe");
    if !jre_java.exists() {
        // 优先用 server.json 里的 URL，否则用默认 Adoptium 地址
        let jre_url = downloads
            .jre_url
            .as_deref()
            .unwrap_or(config::DEFAULT_JRE_URL);

        on_progress(Progress::new(5, "正在下载 Java 运行时..."));
        let zip_path = base_dir.join("updater/jre-download.zip");
        download_file(jre_url, &zip_path, on_progress, 5, 28)?;

        on_progress(Progress::new(28, "正在解压 Java..."));
        let jre_dir = base_dir.join("jre");
        extract_zip_strip_toplevel(&zip_path, &jre_dir)
            .context("解压 JRE 失败")?;

        // 清理下载的 zip
        fs::remove_file(&zip_path).ok();

        // 验证解压成功
        if !jre_java.exists() {
            bail!("JRE 解压后找不到 java.exe，请联系管理员");
        }
    }
    on_progress(Progress::new(30, "Java 就绪"));

    // ── 下载 PCL2（如果不存在） ──
    let pcl2_path = base_dir.join(config::PCL2_EXE);
    if !pcl2_path.exists() {
        let pcl2_url = downloads
            .pcl2_url
            .as_deref()
            .context("server.json 中未配置 PCL2 下载地址 (downloads.pcl2_url)")?;

        on_progress(Progress::new(31, "正在下载启动器..."));
        download_file(pcl2_url, &pcl2_path, on_progress, 31, 38)?;
    }
    on_progress(Progress::new(38, "启动器就绪"));

    // ── 下载 packwiz-installer-bootstrap.jar（如果不存在） ──
    let packwiz_jar = base_dir.join(config::PACKWIZ_BOOTSTRAP_JAR);
    if !packwiz_jar.exists() {
        let packwiz_url = downloads
            .packwiz_bootstrap_url
            .as_deref()
            .unwrap_or(config::DEFAULT_PACKWIZ_BOOTSTRAP_URL);

        on_progress(Progress::new(39, "正在下载模组同步器..."));
        download_file(packwiz_url, &packwiz_jar, on_progress, 39, 42)?;
    }
    on_progress(Progress::new(42, "模组同步器就绪"));

    // ── 下载 fabric-installer.jar（如果不存在） ──
    let fabric_jar = base_dir.join(config::FABRIC_INSTALLER_JAR);
    if !fabric_jar.exists() {
        let fabric_url = downloads
            .fabric_installer_url
            .as_deref()
            .context("server.json 中未配置 Fabric 安装器下载地址 (downloads.fabric_installer_url)")?;

        on_progress(Progress::new(43, "正在下载 Fabric 安装器..."));
        download_file(fabric_url, &fabric_jar, on_progress, 43, 46)?;
    }
    on_progress(Progress::new(46, "Fabric 安装器就绪"));

    // ── 生成 PCL2 Setup.ini ──
    let setup_ini = base_dir.join("PCL/Setup.ini");
    if !setup_ini.exists() {
        on_progress(Progress::new(47, "正在配置启动器..."));
        fs::write(&setup_ini, config::PCL2_SETUP_INI)
            .context("写入 Setup.ini 失败")?;
    }

    // ── 下载并解压默认设置包（仅首次） ──
    let settings_marker = base_dir.join("updater/.settings_installed");
    if !settings_marker.exists() {
        if let Some(ref settings_url) = downloads.settings_url {
            on_progress(Progress::new(48, "正在下载默认设置..."));
            let zip_path = base_dir.join("updater/settings-download.zip");
            download_file(settings_url, &zip_path, on_progress, 48, 49)?;

            on_progress(Progress::new(49, "正在应用默认设置..."));
            let mc_dir = base_dir.join(config::MINECRAFT_DIR);
            fs::create_dir_all(&mc_dir).context("创建 .minecraft 目录失败")?;
            extract_settings_zip(&zip_path, &mc_dir)
                .context("解压设置包失败")?;

            // 清理下载的 zip
            fs::remove_file(&zip_path).ok();
        }
        // 写入标记文件，防止后续运行重复解压覆盖玩家设置
        fs::write(&settings_marker, "installed")
            .context("写入设置安装标记失败")?;
    }

    on_progress(Progress::new(50, "首次安装完成"));
    Ok(())
}

// ────────────────────────────────────────────────────────────
// 工具函数
// ────────────────────────────────────────────────────────────

/// 下载文件并报告进度。
///
/// progress_start / progress_end 定义了这次下载在总进度条中占的范围。
/// 例如 start=5, end=28 表示从 5% 到 28%。
fn download_file(
    url: &str,
    dest: &Path,
    on_progress: &dyn Fn(Progress),
    progress_start: u32,
    progress_end: u32,
) -> Result<()> {
    // 确保目标目录存在
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    // 使用较长超时（大文件可能需要几分钟）
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(config::DOWNLOAD_TIMEOUT_SECS))
        .build();

    let response = agent
        .get(url)
        .call()
        .with_context(|| format!("下载失败: {}", url))?;

    // 尝试获取文件大小（用于进度百分比）
    let total_size = response
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    let mut reader = response.into_reader();
    let mut file = fs::File::create(dest)
        .with_context(|| format!("创建文件失败: {}", dest.display()))?;

    let mut buf = [0u8; 65536]; // 64KB 缓冲区
    let mut downloaded: u64 = 0;

    loop {
        let n = reader.read(&mut buf).context("读取下载数据失败")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("写入文件失败")?;
        downloaded += n as u64;

        // 计算并报告进度
        if total_size > 0 {
            let fraction = downloaded as f64 / total_size as f64;
            let pct = progress_start
                + (fraction * (progress_end - progress_start) as f64) as u32;
            let mb_done = downloaded as f64 / 1_048_576.0;
            let mb_total = total_size as f64 / 1_048_576.0;
            on_progress(Progress::new(
                pct.min(progress_end),
                format!("下载中... {:.1}/{:.1} MB", mb_done, mb_total),
            ));
        }
    }

    Ok(())
}

/// 解压 ZIP 文件，自动去掉顶层目录。
///
/// 许多工具的 ZIP 包有一个顶层目录（如 `jdk-21.0.5+11-jre/`），
/// 里面才是实际文件。此函数会检测并去掉这个前缀，
/// 使内容直接解压到 dest 目录下。
///
/// 例如：ZIP 内 `jdk-21.0.5+11-jre/bin/java.exe`
/// → 解压为 `dest/bin/java.exe`
fn extract_zip_strip_toplevel(zip_path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(zip_path)
        .with_context(|| format!("打开 ZIP 失败: {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("读取 ZIP 文件失败")?;

    // 检测是否所有条目都有相同的顶层目录前缀
    let common_prefix = detect_common_prefix(&mut archive);

    fs::create_dir_all(dest)?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("读取 ZIP 条目 #{} 失败", i))?;

        let full_name = entry.name().to_string();

        // 去掉顶层目录前缀
        let relative = if let Some(ref prefix) = common_prefix {
            let stripped = full_name
                .strip_prefix(prefix)
                .unwrap_or(&full_name);
            stripped.trim_start_matches('/')
        } else {
            &full_name
        };

        // 跳过空路径（顶层目录本身）
        if relative.is_empty() {
            continue;
        }

        let out_path = dest.join(relative);

        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut outfile = fs::File::create(&out_path)
                .with_context(|| format!("创建文件失败: {}", out_path.display()))?;
            std::io::copy(&mut entry, &mut outfile)?;
        }
    }

    Ok(())
}

/// 检测 ZIP 文件中所有条目是否共享一个顶层目录前缀。
///
/// 如果是（例如所有文件都在 `jdk-21.0.5+11-jre/` 下），
/// 返回该前缀（含尾部 `/`）。否则返回 None。
fn detect_common_prefix(archive: &mut zip::ZipArchive<fs::File>) -> Option<String> {
    if archive.len() == 0 {
        return None;
    }

    // 取第一个条目的第一级目录名
    let candidate = {
        let first = archive.by_index(0).ok()?;
        let first_name = first.name().to_string();
        let c = first_name.split('/').next()?.to_string();
        c
        // first 在这里被 drop，释放对 archive 的借用
    };

    if candidate.is_empty() {
        return None;
    }

    let prefix = format!("{}/", candidate);

    // 检查所有条目是否都以这个前缀开头
    for i in 1..archive.len() {
        let name = {
            let entry = archive.by_index(i).ok()?;
            entry.name().to_string()
        };
        if !name.starts_with(&prefix) {
            return None;
        }
    }

    Some(prefix)
}

/// 解压设置包 ZIP 到目标目录（不去除顶层目录）。
///
/// 设置包内的文件应直接映射到 `.minecraft/` 的目录结构，例如：
///   options.txt  → .minecraft/options.txt
///   servers.dat  → .minecraft/servers.dat
///   config/      → .minecraft/config/
///
/// 不会覆盖已存在的文件，以保留玩家的个人设置。
fn extract_settings_zip(zip_path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(zip_path)
        .with_context(|| format!("打开设置包 ZIP 失败: {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("读取设置包 ZIP 失败")?;

    fs::create_dir_all(dest)?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("读取设置包条目 #{} 失败", i))?;

        let name = entry.name().to_string();
        if name.is_empty() {
            continue;
        }

        let out_path = dest.join(&name);

        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
        } else {
            // 不覆盖已有文件，保护玩家现有设置
            if out_path.exists() {
                continue;
            }
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut outfile = fs::File::create(&out_path)
                .with_context(|| format!("创建设置文件失败: {}", out_path.display()))?;
            std::io::copy(&mut entry, &mut outfile)?;
        }
    }

    Ok(())
}
