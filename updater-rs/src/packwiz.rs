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

/// Windows: 不创建控制台窗口
const CREATE_NO_WINDOW: u32 = 0x08000000;

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
pub fn sync_modpack(base_dir: &Path, pack_url: &str) -> Result<()> {
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
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("启动 packwiz-installer 失败")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "模组同步失败 (exit code: {:?}):\nstdout: {}\nstderr: {}",
            output.status.code(),
            stdout,
            stderr
        );
    }

    Ok(())
}
