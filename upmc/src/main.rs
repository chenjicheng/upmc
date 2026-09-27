// ============================================================
// main.rs — 停用版程序入口
// ============================================================
// 保留自更新 helper 和健康确认，使现有安装能够安全升级。
// 普通启动只显示停用通知，不进入旧版安装或游戏流程。
// ============================================================

// 在 release 模式下隐藏控制台黑框
// 这个属性让 Windows 不会弹出 cmd 窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bootstrap;
mod config;
mod discord_proxy;
mod fabric;
#[cfg(test)]
mod gui;
mod gui_state;
mod gui_switches;
mod java;
#[cfg(test)]
mod java_test_fixture;
mod managed_mods;
mod observability;
mod packwiz;
mod retirement;
mod retry;
mod selfupdate;
mod update;
mod version;
mod xray;

#[cfg(test)]
use config::{ChannelConfig, UpdateChannel};
#[cfg(test)]
use std::path::PathBuf;

fn main() {
    // 自更新 helper 模式必须最先处理，避免 helper 初始化 GUI 或执行正常更新流程。
    match selfupdate::try_run_update_helper_from_args() {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            observability::event(
                "startup.fatal",
                "self-update helper failed",
                format!("{e:#}"),
                "apply-self-update",
                "exit with failure",
                std::process::id(),
            );
            eprintln!("自更新 helper 执行失败: {e:#}");
            std::process::exit(1);
        }
    }

    let startup_health = match selfupdate::startup_health_ack_from_args() {
        Ok(value) => value,
        Err(error) => {
            observability::event(
                "startup.fatal",
                "invalid startup health arguments",
                format!("{error:#}"),
                "startup health acknowledgement",
                "exit with failure",
                std::process::id(),
            );
            std::process::exit(1);
        }
    };
    // A supervised candidate preserves all active transaction files for its helper.
    if startup_health.is_none() {
        selfupdate::cleanup_old_exe();
    }

    if let Some(ack) = startup_health {
        selfupdate::acknowledge_health_when_window_ready(ack);
    }

    // The retirement release never enters the old update, proxy, or game flow.
    if let Err(error) = retirement::run() {
        eprintln!("停用提示窗口启动失败：{error}");
        std::process::exit(1);
    }
}

/// 解析更新通道。
///
/// 优先级：命令行参数 > channel.json（设置窗口保存） > 编译期默认值
#[cfg(test)]
fn resolve_channel(base_dir: &std::path::Path) -> ChannelConfig {
    let args: Vec<String> = std::env::args().collect();
    let mut cli_channel: Option<UpdateChannel> = None;

    let mut i = 1;
    while i < args.len() {
        if args[i] == "--channel" {
            if let Some(val) = args.get(i + 1) {
                match val.to_lowercase().as_str() {
                    "dev" => cli_channel = Some(UpdateChannel::Dev),
                    "stable" => cli_channel = Some(UpdateChannel::Stable),
                    other => {
                        eprintln!("未知通道: {other}，使用编译期默认值");
                    }
                }
                i += 2;
                continue;
            }
        }
        i += 1;
    }

    // 命令行 > channel.json > 编译期默认值
    let path = base_dir.join(config::CHANNEL_CONFIG_FILE);
    let (channel, from_file) = if let Some(ch) = cli_channel {
        (ch, false)
    } else {
        match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str::<ChannelConfig>(&s) {
                Ok(cfg) => (cfg.channel, true),
                Err(e) => {
                    eprintln!("警告: channel.json 解析失败，使用默认通道: {e}");
                    (UpdateChannel::COMPILED_DEFAULT, false)
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (UpdateChannel::COMPILED_DEFAULT, false)
            }
            Err(e) => {
                eprintln!("警告: 读取 channel.json 失败，使用默认通道: {e}");
                (UpdateChannel::COMPILED_DEFAULT, false)
            }
        }
    };

    let cfg = ChannelConfig { channel };
    // 仅在 CLI 指定或文件不存在时写入，避免覆盖损坏的配置
    if cli_channel.is_some() || !from_file {
        if let Err(e) = config::save_channel_config(base_dir, &cfg) {
            eprintln!("保存通道配置失败: {e:#}");
        }
    }
    cfg
}

/// 获取组件安装的基准目录。
///
/// 返回用户文档文件夹下的 `CJC整合包/` 子目录。
/// 例如：`C:\Users\<用户>\Documents\CJC整合包\`
///
/// 如果检测到旧版安装目录（exe 同级的 CJC整合包/），
/// 会自动将其迁移到文档文件夹。
#[cfg(test)]
fn get_base_dir() -> PathBuf {
    let new_dir = config::get_install_dir();
    let legacy_dir = config::get_legacy_install_dir();

    // 新旧路径相同时无需迁移（exe 本身就在文档文件夹中）
    if legacy_dir == new_dir {
        return new_dir;
    }

    // 新旧目录都存在 → 使用新目录，提示用户可清理旧目录
    if legacy_dir.exists() && new_dir.exists() {
        eprintln!(
            "新旧安装目录同时存在，使用新位置: {}\n\
             旧目录可手动删除: {}",
            new_dir.display(),
            legacy_dir.display()
        );
        return new_dir;
    }

    // 旧目录存在且新目录不存在 → 迁移
    if legacy_dir.exists() && !new_dir.exists() {
        eprintln!(
            "检测到旧版安装，正在迁移: {} → {}",
            legacy_dir.display(),
            new_dir.display()
        );

        // 确保新目录的父目录存在
        if let Some(parent) = new_dir.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("创建目标目录失败: {e}");
            }
        }

        // 尝试 rename（同盘符下是原子操作，速度极快）
        match std::fs::rename(&legacy_dir, &new_dir) {
            Ok(()) => {
                eprintln!("迁移成功");
            }
            Err(e) => {
                // rename 失败（跨盘符等），回退到使用旧目录
                eprintln!(
                    "迁移失败（将继续使用旧位置）: {e}\n\
                     旧位置: {}\n新位置: {}",
                    legacy_dir.display(),
                    new_dir.display()
                );
                return legacy_dir;
            }
        }
    }

    new_dir
}

#[cfg(test)]
mod transition_channel_tests {
    use super::*;

    #[test]
    fn persisted_legacy_dev_channel_survives_unflagged_first_hop() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(config::CHANNEL_CONFIG_FILE);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = "{\"channel\":\"dev\",\"dev_build_id\":\"legacy-value\"}";
        std::fs::write(&path, legacy).unwrap();
        let selected = resolve_channel(dir.path());
        assert_eq!(selected.channel, UpdateChannel::Dev);
        assert_eq!(
            config::updater_version_url(selected.channel),
            "https://upmc.chenjicheng.cn/bridge/dev/version.json"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), legacy);
    }

    #[test]
    fn saved_channel_is_reloaded_for_both_release_channels() {
        let dir = tempfile::TempDir::new().unwrap();
        for channel in [UpdateChannel::Stable, UpdateChannel::Dev] {
            config::save_channel_config(dir.path(), &ChannelConfig { channel }).unwrap();
            assert_eq!(resolve_channel(dir.path()).channel, channel);
        }
    }
}
