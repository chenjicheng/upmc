//! Packwiz downloads into updater-owned storage. Only explicitly tracked JARs
//! may be reconciled into the game directory; player data never goes to Java.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::config;

pub(crate) const WORKSPACE: &str = "updater/packwiz-managed";
pub(crate) const MANIFEST: &str = "updater/managed-mods.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct FileHash {
    #[serde(rename = "type")]
    kind: String,
    value: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ModFile {
    path: String,
    hash: FileHash,
}

#[derive(Default, Deserialize, Serialize)]
struct Manifest {
    files: BTreeMap<String, ModFile>,
}

/// Reject traversal, Windows aliases and reparse points before touching files.
pub(crate) fn safe_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    ensure!(!relative.as_os_str().is_empty(), "文件路径为空");
    let mut full = root.to_path_buf();
    reject_link(&full)?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            bail!("拒绝越界路径：{}", relative.display());
        };
        let text = name.to_str().context("文件路径不是有效 Unicode")?;
        let stem = text.split('.').next().unwrap_or("").to_ascii_uppercase();
        ensure!(
            !text.contains([':', '\\', '/', '\0'])
                && !text.ends_with(['.', ' '])
                && !text.chars().any(char::is_control)
                && !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && stem.as_bytes()[3].is_ascii_digit()),
            "拒绝不安全的文件名：{text}"
        );
        full.push(name);
        reject_link(&full)?;
    }
    Ok(full)
}

fn reject_link(path: &Path) -> Result<()> {
    match path.symlink_metadata() {
        Ok(meta) => ensure!(
            meta.file_attributes() & 0x400 == 0,
            "为保护玩家文件，更新不跟随链接或目录联接：{}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("读取文件信息失败：{}", path.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an explicitly prepared official Packwiz acceptance fixture"]
    fn official_packwiz_manifest_migrates_and_preserves_player_files() {
        let base = PathBuf::from(std::env::var_os("UPMC_PACKWIZ_ACCEPTANCE_ROOT").unwrap());
        assert_eq!(
            fs::read(base.join(".upmc-test-root")).unwrap(),
            b"UPMC_TEST_FIXTURE"
        );
        let game = base.join(config::MINECRAFT_DIR);
        for path in [
            "options.txt",
            "config/litematica.json",
            "mods/player-extra.jar",
            "versions/old/PCL/Setup.ini",
        ] {
            write(&game, path, b"player bytes");
        }
        apply(&base, false).unwrap();
        let installed = previous(&base).unwrap();
        assert!(!installed.files.is_empty());
        for entry in installed.files.values() {
            assert!(matches_hash(&game.join(&entry.path), &entry.hash).unwrap());
        }
        // Exercise a real 0.5.8 manifest migration, including sha512 Modrinth JARs.
        fs::copy(
            base.join(WORKSPACE).join("packwiz.json"),
            game.join("packwiz.json"),
        )
        .unwrap();
        fs::remove_file(base.join(MANIFEST)).unwrap();
        apply(&base, false).unwrap();
        for path in [
            "options.txt",
            "config/litematica.json",
            "mods/player-extra.jar",
            "versions/old/PCL/Setup.ini",
        ] {
            assert_eq!(fs::read(game.join(path)).unwrap(), b"player bytes");
        }
        println!(
            "Verified {} official managed mods and legacy ownership migration",
            installed.files.len()
        );
    }

    fn hash(bytes: &[u8]) -> FileHash {
        FileHash {
            kind: "sha256".into(),
            value: format!("{:x}", Sha256::digest(bytes)),
        }
    }

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".minecraft/mods")).unwrap();
        prepare_workspace(root.path()).unwrap();
        root
    }

    fn write(root: &Path, path: &str, bytes: &[u8]) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn downloaded(base: &Path, files: &[(&str, &str, &[u8])]) {
        let workspace = base.join(WORKSPACE);
        let mut cached = serde_json::Map::new();
        for (id, path, content) in files {
            write(&workspace, path, content);
            cached.insert(
                id.to_string(),
                serde_json::json!({
                    "cachedLocation": path, "linkedFileHash": hash(content)
                }),
            );
        }
        write(&workspace, "packwiz.json", &serde_json::to_vec(&serde_json::json!({
            "cachedFiles": cached, "packFileHash": hash(b"pack"), "indexFileHash": hash(b"index")
        })).unwrap());
    }

    fn tracked(base: &Path, files: &[(&str, &str, &[u8])]) {
        let files = files
            .iter()
            .map(|(id, path, content)| {
                (
                    id.to_string(),
                    ModFile {
                        path: path.to_string(),
                        hash: hash(content),
                    },
                )
            })
            .collect();
        write(
            base,
            MANIFEST,
            &serde_json::to_vec(&Manifest { files }).unwrap(),
        );
    }

    #[test]
    fn updates_managed_mods_without_touching_player_content() {
        let root = fixture();
        let base = root.path();
        write(base, ".minecraft/mods/old.jar", b"old managed");
        write(base, ".minecraft/mods/my-extra.jar", b"player mod");
        let player_files = [
            "config/litematica.json",
            "options.txt",
            "servers.dat",
            "saves/My world/level.dat",
            "resourcepacks/custom.zip",
            "screenshots/shot.png",
            "versions/old/PCL/Setup.ini",
            "versions/old/config/tweakeroo.json",
        ];
        for path in player_files {
            write(&base.join(".minecraft"), path, b"player data");
        }
        tracked(
            base,
            &[("mods/managed.pw.toml", "mods/old.jar", b"old managed")],
        );
        downloaded(
            base,
            &[
                ("mods/managed.pw.toml", "mods/new.jar", b"new managed"),
                (
                    "config/litematica.json",
                    "config/litematica.json",
                    b"server defaults",
                ),
                ("options.txt", "options.txt", b"server keybindings"),
            ],
        );
        apply(base, false).unwrap();
        assert_eq!(
            fs::read(base.join(".minecraft/mods/new.jar")).unwrap(),
            b"new managed"
        );
        assert!(!base.join(".minecraft/mods/old.jar").exists());
        assert_eq!(
            fs::read(base.join(".minecraft/mods/my-extra.jar")).unwrap(),
            b"player mod"
        );
        for path in player_files {
            assert_eq!(
                fs::read(base.join(".minecraft").join(path)).unwrap(),
                b"player data",
                "{path}"
            );
        }
    }

    #[test]
    fn missing_player_settings_are_not_recreated_on_update() {
        let root = fixture();
        downloaded(root.path(), &[("options.txt", "options.txt", b"defaults")]);
        apply(root.path(), false).unwrap();
        assert!(!root.path().join(".minecraft/options.txt").exists());
    }

    #[test]
    fn first_install_defaults_never_replace_existing_settings() {
        let root = fixture();
        write(root.path(), ".minecraft/options.txt", b"player keys");
        downloaded(
            root.path(),
            &[
                ("options.txt", "options.txt", b"defaults"),
                ("config/new.json", "config/new.json", b"first defaults"),
            ],
        );
        apply(root.path(), true).unwrap();
        assert_eq!(
            fs::read(root.path().join(".minecraft/options.txt")).unwrap(),
            b"player keys"
        );
        assert_eq!(
            fs::read(root.path().join(".minecraft/config/new.json")).unwrap(),
            b"first defaults"
        );
    }

    #[test]
    fn same_name_player_jar_conflict_prevents_all_game_writes() {
        let root = fixture();
        write(root.path(), ".minecraft/mods/a.jar", b"old");
        write(root.path(), ".minecraft/mods/z.jar", b"player extra");
        tracked(root.path(), &[("a", "mods/a.jar", b"old")]);
        downloaded(
            root.path(),
            &[("a", "mods/a.jar", b"new"), ("z", "mods/z.jar", b"server")],
        );
        assert!(apply(root.path(), false).is_err());
        assert_eq!(
            fs::read(root.path().join(".minecraft/mods/a.jar")).unwrap(),
            b"old"
        );
        assert_eq!(
            fs::read(root.path().join(".minecraft/mods/z.jar")).unwrap(),
            b"player extra"
        );
    }

    #[test]
    fn identical_untracked_player_mod_is_not_adopted() {
        let root = fixture();
        write(root.path(), ".minecraft/mods/player.jar", b"same bytes");
        downloaded(root.path(), &[("player", "mods/player.jar", b"same bytes")]);
        assert!(apply(root.path(), false).is_err());
        assert!(!root.path().join(MANIFEST).exists());
        assert_eq!(
            fs::read(root.path().join(".minecraft/mods/player.jar")).unwrap(),
            b"same bytes"
        );
    }

    #[test]
    fn unicode_case_only_mod_rename_keeps_the_installed_file() {
        let root = fixture();
        write(root.path(), ".minecraft/mods/Ä.jar", b"same");
        tracked(root.path(), &[("a", "mods/Ä.jar", b"same")]);
        downloaded(root.path(), &[("a", "mods/ä.jar", b"same")]);
        apply(root.path(), false).unwrap();
        assert_eq!(
            fs::read(root.path().join(".minecraft/mods/ä.jar")).unwrap(),
            b"same"
        );
    }

    #[test]
    fn unicode_expansion_is_not_a_windows_filename_alias() {
        let root = fixture();
        let base = root.path();
        write(base, ".minecraft/mods/ß.jar", b"old");
        tracked(base, &[("a", "mods/ß.jar", b"old")]);
        downloaded(base, &[("a", "mods/ss.jar", b"new")]);
        apply(base, false).unwrap();
        assert_eq!(
            fs::read(base.join(".minecraft/mods/ss.jar")).unwrap(),
            b"new"
        );
        assert!(!base.join(".minecraft/mods/ß.jar").exists());
    }

    #[test]
    fn unicode_expansion_does_not_grant_ownership_of_a_player_mod() {
        let root = fixture();
        let base = root.path();
        write(base, ".minecraft/mods/ß.jar", b"old");
        write(base, ".minecraft/mods/ss.jar", b"old");
        tracked(base, &[("a", "mods/ß.jar", b"old")]);
        downloaded(base, &[("a", "mods/ss.jar", b"new")]);
        assert!(apply(base, false).is_err());
        assert_eq!(
            fs::read(base.join(".minecraft/mods/ss.jar")).unwrap(),
            b"old"
        );
        assert_eq!(
            fs::read(base.join(".minecraft/mods/ß.jar")).unwrap(),
            b"old"
        );
    }

    #[test]
    fn incomplete_first_install_retries_defaults_without_replacing_player_changes() {
        let root = tempfile::tempdir().unwrap();
        assert!(crate::bootstrap::begin_first_install(root.path()).unwrap());
        write(root.path(), ".minecraft/options.txt", b"player keys");
        assert!(crate::bootstrap::begin_first_install(root.path()).unwrap());
        create_default(
            &root.path().join(".minecraft"),
            Path::new("options.txt"),
            b"defaults",
        )
        .unwrap();
        assert_eq!(
            fs::read(root.path().join(".minecraft/options.txt")).unwrap(),
            b"player keys"
        );
    }

    #[test]
    fn pending_install_or_mod_transaction_cannot_launch_offline() {
        let root = fixture();
        write(root.path(), config::PCL2_EXE, b"launcher");
        write(root.path(), config::LOCAL_VERSION_FILE, b"{}");
        assert!(crate::bootstrap::is_bootstrapped(root.path()));
        write(root.path(), "updater/.initial_install_started", b"pending");
        assert!(!crate::bootstrap::is_bootstrapped(root.path()));
        write(root.path(), "updater/.initial_install_started", b"complete");
        write(root.path(), "updater/mod-transaction.json", b"pending");
        assert!(!crate::bootstrap::is_bootstrapped(root.path()));
    }

    #[test]
    fn changed_managed_mod_is_preserved_when_remote_removes_it() {
        let root = fixture();
        tracked(root.path(), &[("a", "mods/a.jar", b"original")]);
        write(root.path(), ".minecraft/mods/a.jar", b"player modified");
        downloaded(root.path(), &[]);
        assert!(apply(root.path(), false).is_err());
        assert_eq!(
            fs::read(root.path().join(".minecraft/mods/a.jar")).unwrap(),
            b"player modified"
        );
    }

    #[test]
    fn deleting_markers_does_not_make_an_existing_game_a_new_install() {
        let root = tempfile::tempdir().unwrap();
        assert!(crate::bootstrap::begin_first_install(root.path()).unwrap());
        crate::bootstrap::finish_first_install(root.path()).unwrap();
        assert!(!crate::bootstrap::begin_first_install(root.path()).unwrap());
        write(root.path(), ".minecraft/config/keys.json", b"player keys");
        fs::remove_file(root.path().join("updater/.initial_install_started")).unwrap();
        assert!(!crate::bootstrap::begin_first_install(root.path()).unwrap());
        assert_eq!(
            fs::read(root.path().join(".minecraft/config/keys.json")).unwrap(),
            b"player keys"
        );
    }

    #[test]
    fn incomplete_packwiz_download_never_reaches_player_directory() {
        let root = fixture();
        write(root.path(), ".minecraft/mods/a.jar", b"old");
        tracked(root.path(), &[("a", "mods/a.jar", b"old")]);
        downloaded(root.path(), &[("a", "mods/a.jar", b"new")]);
        let path = root.path().join(WORKSPACE).join("packwiz.json");
        let mut value = read_packwiz(&path).unwrap();
        value["packFileHash"] = serde_json::Value::Null;
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(apply(root.path(), false).is_err());
        assert_eq!(
            fs::read(root.path().join(".minecraft/mods/a.jar")).unwrap(),
            b"old"
        );
    }

    #[test]
    fn resumes_interrupted_transaction_before_applying_a_newer_pack() {
        let root = fixture();
        let base = root.path();
        // The previous process published one new mod, then exited before its manifest.
        write(base, ".minecraft/mods/a.jar", b"intermediate");
        let entry = ModFile {
            path: "mods/a.jar".into(),
            hash: hash(b"intermediate"),
        };
        write(
            base,
            "updater/mod-transaction.json",
            &serde_json::to_vec(&serde_json::json!({
                "old": {"files": {}}, "next": {"files": {"a": entry}},
                "directory": "updater/mod-transactions/fixture"
            }))
            .unwrap(),
        );
        downloaded(base, &[("a", "mods/a.jar", b"latest")]);
        apply(base, false).unwrap();
        assert_eq!(
            fs::read(base.join(".minecraft/mods/a.jar")).unwrap(),
            b"latest"
        );
        assert!(!base.join("updater/mod-transaction.json").exists());
    }

    #[test]
    fn manifest_publication_failure_recovers_without_adopting_unknown_mods() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = fixture();
        let base = root.path();
        write(base, ".minecraft/mods/old.jar", b"old");
        tracked(base, &[("a", "mods/old.jar", b"old")]);
        downloaded(base, &[("a", "mods/new.jar", b"new")]);
        // Read is permitted, but replacing the manifest is blocked.
        let locked = OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(base.join(MANIFEST))
            .unwrap();
        assert!(apply(base, false).is_err());
        assert_eq!(
            fs::read(base.join(".minecraft/mods/new.jar")).unwrap(),
            b"new"
        );
        assert!(base.join("updater/mod-transaction.json").exists());
        drop(locked);
        apply(base, false).unwrap();
        assert!(!base.join("updater/mod-transaction.json").exists());
        assert_eq!(
            fs::read(base.join(".minecraft/mods/new.jar")).unwrap(),
            b"new"
        );
    }

    #[test]
    fn locked_old_mod_is_preserved_and_transaction_retries_after_unlock() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = fixture();
        let base = root.path();
        let target = base.join(".minecraft/mods/a.jar");
        write(base, ".minecraft/mods/a.jar", b"old");
        tracked(base, &[("a", "mods/a.jar", b"old")]);
        downloaded(base, &[("a", "mods/a.jar", b"new")]);
        let locked = OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&target)
            .unwrap();
        assert!(apply(base, false).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        drop(locked);
        apply(base, false).unwrap();
        assert_eq!(fs::read(target).unwrap(), b"new");
    }

    #[test]
    fn corrupt_staged_mod_does_not_touch_existing_game_file() {
        let root = fixture();
        let base = root.path();
        write(base, ".minecraft/mods/a.jar", b"old");
        tracked(base, &[("a", "mods/a.jar", b"old")]);
        downloaded(base, &[("a", "mods/a.jar", b"new")]);
        write(&base.join(WORKSPACE), "mods/a.jar", b"truncated");
        assert!(apply(base, false).is_err());
        assert_eq!(
            fs::read(base.join(".minecraft/mods/a.jar")).unwrap(),
            b"old"
        );
        assert!(!base.join("updater/mod-transaction.json").exists());
    }

    #[test]
    fn packwiz_workspace_junction_is_rejected_before_java_runs() {
        use std::os::windows::process::CommandExt;
        let root = fixture();
        let player = root.path().join(".minecraft/config");
        fs::create_dir_all(&player).unwrap();
        let link = root.path().join(WORKSPACE).join("config");
        let output = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link.to_str().unwrap().replace('/', "\\"))
            .arg(player.to_str().unwrap().replace('/', "\\"))
            .creation_flags(config::CREATE_NO_WINDOW)
            .output()
            .unwrap();
        assert!(output.status.success(), "junction fixture: {:?}", output);
        let result = prepare_workspace(root.path());
        fs::remove_dir(&link).unwrap();
        assert!(result.is_err());
    }
}

fn mod_path(value: &str) -> Result<String> {
    let value = value.replace('\\', "/");
    let pieces: Vec<_> = value.split('/').collect();
    ensure!(
        pieces.len() == 2
            && pieces[0] == "mods"
            && pieces[1].to_ascii_lowercase().ends_with(".jar"),
        "受管理内容必须是 mods/ 中的 JAR：{value}"
    );
    // safe_path performs the remaining platform-specific checks at use time.
    Ok(value)
}

fn matches_hash(path: &Path, expected: &FileHash) -> Result<bool> {
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "不是普通文件：{}",
        path.display()
    );
    let mut buf = [0u8; 65536];
    let actual = match expected.kind.as_str() {
        "sha256" => {
            let mut hash = Sha256::new();
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hash.update(&buf[..n]);
            }
            format!("{:x}", hash.finalize())
        }
        "sha512" => {
            let mut hash = Sha512::new();
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hash.update(&buf[..n]);
            }
            format!("{:x}", hash.finalize())
        }
        _ => bail!(
            "无法确认旧模组摘要 {}，已保留玩家文件：{}",
            expected.kind,
            path.display()
        ),
    };
    Ok(actual.eq_ignore_ascii_case(&expected.value))
}

fn read_packwiz(path: &Path) -> Result<serde_json::Value> {
    serde_json::from_slice(&fs::read(path)?).context("读取 Packwiz 安装记录失败")
}

fn records(value: &serde_json::Value) -> Result<Manifest> {
    let entries = value["cachedFiles"]
        .as_object()
        .context("Packwiz 安装记录缺少 cachedFiles")?;
    let mut files = BTreeMap::new();
    for (id, entry) in entries {
        let Some(location) = entry["cachedLocation"].as_str() else {
            continue;
        };
        let Ok(path) = mod_path(location) else {
            continue;
        };
        let hash = entry
            .get("linkedFileHash")
            .filter(|hash| !hash.is_null())
            .or_else(|| entry.get("hash"))
            .context("模组缺少下载摘要")?;
        files.insert(
            id.clone(),
            ModFile {
                path,
                hash: serde_json::from_value(hash.clone()).context("模组摘要格式错误")?,
            },
        );
    }
    Ok(Manifest { files })
}

fn previous(base_dir: &Path) -> Result<Manifest> {
    let path = safe_path(base_dir, Path::new(MANIFEST))?;
    if path.exists() {
        let mut manifest: Manifest = serde_json::from_slice(&fs::read(path)?)
            .context("受管理模组记录损坏，已停止更新以保护玩家文件")?;
        for file in manifest.files.values_mut() {
            file.path = mod_path(&file.path)?;
        }
        return Ok(manifest);
    }
    let legacy = safe_path(
        base_dir,
        &Path::new(config::MINECRAFT_DIR).join("packwiz.json"),
    )?;
    if legacy.is_file() {
        records(&read_packwiz(&legacy)?)
    } else {
        Ok(Manifest::default())
    }
}

fn nonce() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

/// Windows rename without REPLACE_EXISTING: an intervening player file wins.
fn move_new(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::winbase::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<_> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Replace updater-owned metadata only, after the complete bytes are durable.
pub(crate) fn write_owned(path: &Path, bytes: &[u8]) -> Result<()> {
    reject_link(path)?;
    let temporary = path.with_extension(format!("{}.pending", nonce()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

/// Complete bytes are staged before publishing a create-only default.
pub(crate) fn create_default(root: &Path, relative: &Path, bytes: &[u8]) -> Result<()> {
    let path = safe_path(root, relative)?;
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(path.parent().context("默认文件没有父目录")?)?;
    let temporary = path.with_extension(format!("{}.pending", nonce()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        match move_new(&temporary, &path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error).context("创建默认文件失败"),
        }
    })();
    let _ = fs::remove_file(&temporary);
    result
}

pub(crate) fn reject_link_tree(root: &Path) -> Result<()> {
    reject_link(root)?;
    if root.is_dir() {
        for entry in fs::read_dir(root)? {
            reject_link_tree(&entry?.path())?;
        }
    }
    Ok(())
}

pub(crate) fn prepare_workspace(base_dir: &Path) -> Result<PathBuf> {
    let workspace = safe_path(base_dir, Path::new(WORKSPACE))?;
    fs::create_dir_all(&workspace)?;
    reject_link_tree(&workspace)?;
    Ok(workspace)
}

const JOURNAL: &str = "updater/mod-transaction.json";

#[derive(Deserialize, Serialize)]
struct Transaction {
    old: Manifest,
    next: Manifest,
    directory: String,
}

// Use Windows ordinal comparison. Unicode uppercase expansion would incorrectly
// grant ownership of ss.jar through an unrelated managed ß.jar.
fn same_path(left: &str, right: &str) -> Result<bool> {
    let left: Vec<u16> = left.encode_utf16().collect();
    let right: Vec<u16> = right.encode_utf16().collect();
    let result = unsafe {
        winapi::um::stringapiset::CompareStringOrdinal(
            left.as_ptr(),
            left.len().try_into()?,
            right.as_ptr(),
            right.len().try_into()?,
            1,
        )
    };
    ensure!(result != 0, "Windows 文件名比较失败");
    Ok(result == 2)
}

fn contains_path(paths: &[String], path: &str) -> Result<bool> {
    for other in paths {
        if same_path(other, path)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn owns(manifest: &Manifest, path: &str, target: &Path) -> Result<bool> {
    for entry in manifest.files.values() {
        if same_path(&entry.path, path)? && matches_hash(target, &entry.hash)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn check_plan(
    game: &Path,
    old: &Manifest,
    next: &Manifest,
    recovering: bool,
) -> Result<Vec<String>> {
    let mut destinations = Vec::new();
    for entry in next.files.values() {
        let path = mod_path(&entry.path)?;
        ensure!(
            !contains_path(&destinations, &path)?,
            "多个模组使用同一文件名：{path}"
        );
        destinations.push(path.clone());
        let target = safe_path(game, Path::new(&path))?;
        if target.exists() {
            ensure!(
                owns(old, &path, &target)? || (recovering && matches_hash(&target, &entry.hash)?),
                "发现未受管理或已被玩家修改的同名模组，已保留：{path}"
            );
        }
    }
    for prior in old.files.values() {
        let path = mod_path(&prior.path)?;
        let target = safe_path(game, Path::new(&path))?;
        if !contains_path(&destinations, &path)? && target.exists() {
            ensure!(
                owns(old, &path, &target)?,
                "玩家修改过旧模组，已保留：{path}"
            );
        }
    }
    Ok(destinations)
}

fn finish_transaction(base_dir: &Path, transaction: &Transaction) -> Result<()> {
    let game = safe_path(base_dir, Path::new(config::MINECRAFT_DIR))?;
    // A journal can only refer to this updater's transaction storage.
    ensure!(
        transaction
            .directory
            .starts_with("updater/mod-transactions/"),
        "无效的模组事务目录"
    );
    let directory = safe_path(base_dir, Path::new(&transaction.directory))?;
    let destinations = check_plan(&game, &transaction.old, &transaction.next, true)?;
    // Verify every remaining staged payload before moving any old file.
    for entry in transaction.next.files.values() {
        let target = safe_path(&game, Path::new(&entry.path))?;
        if target.exists() && matches_hash(&target, &entry.hash)? {
            continue;
        }
        let staged = safe_path(&directory, &Path::new("new").join(&entry.path))?;
        ensure!(
            matches_hash(&staged, &entry.hash)?,
            "事务中的模组不完整：{}",
            entry.path
        );
    }
    for entry in transaction.next.files.values() {
        let target = safe_path(&game, Path::new(&entry.path))?;
        if target.exists() && matches_hash(&target, &entry.hash)? {
            continue;
        }
        if target.exists() {
            ensure!(
                owns(&transaction.old, &entry.path, &target)?,
                "更新期间模组被修改，已保留：{}",
                entry.path
            );
            let backup = safe_path(&directory, &Path::new("old").join(&entry.path))?;
            fs::create_dir_all(backup.parent().unwrap())?;
            move_new(&target, &backup).context("无法备份受管理模组，请关闭游戏后重试")?;
        }
        let staged = safe_path(&directory, &Path::new("new").join(&entry.path))?;
        fs::create_dir_all(target.parent().unwrap())?;
        // Rename publishes a complete file, never a partially copied JAR.
        move_new(&staged, &target).context("发布模组失败，已保留更新事务及原文件，请重试")?;
    }
    for prior in transaction.old.files.values() {
        if contains_path(&destinations, &prior.path)? {
            continue;
        }
        let target = safe_path(&game, Path::new(&prior.path))?;
        if target.exists() {
            ensure!(
                owns(&transaction.old, &prior.path, &target)?,
                "旧模组在更新期间被修改，已保留：{}",
                prior.path
            );
            let backup = safe_path(&directory, &Path::new("old").join(&prior.path))?;
            fs::create_dir_all(backup.parent().unwrap())?;
            move_new(&target, &backup)?;
        }
    }
    let manifest = safe_path(base_dir, Path::new(MANIFEST))?;
    write_owned(&manifest, &serde_json::to_vec_pretty(&transaction.next)?)?;
    fs::remove_file(safe_path(base_dir, Path::new(JOURNAL))?)?;
    // Retain transaction/old files as recoverable backups.
    Ok(())
}

pub(crate) fn apply(base_dir: &Path, first_install: bool) -> Result<()> {
    let journal = safe_path(base_dir, Path::new(JOURNAL))?;
    if journal.exists() {
        let transaction: Transaction = serde_json::from_slice(&fs::read(&journal)?)
            .context("模组恢复记录损坏，已停止以保护玩家文件")?;
        finish_transaction(base_dir, &transaction)?;
    }
    // Recovery finishes the old transaction first; this call still applies the
    // current download, so the caller cannot cache a newer pack prematurely.
    let workspace = prepare_workspace(base_dir)?;
    let game = safe_path(base_dir, Path::new(config::MINECRAFT_DIR))?;
    let downloaded = read_packwiz(&workspace.join("packwiz.json"))?;
    ensure!(
        downloaded.get("packFileHash").is_some_and(|v| !v.is_null())
            && downloaded
                .get("indexFileHash")
                .is_some_and(|v| !v.is_null()),
        "Packwiz 下载尚未完整成功，玩家目录保持不变"
    );
    let next = records(&downloaded)?;
    let old = previous(base_dir)?;
    check_plan(&game, &old, &next, false)?;
    let directory_name = format!("updater/mod-transactions/{}", nonce());
    let directory = safe_path(base_dir, Path::new(&directory_name))?;
    fs::create_dir_all(directory.parent().unwrap())?;
    fs::create_dir(&directory)?;
    for entry in next.files.values() {
        let target = safe_path(&game, Path::new(&entry.path))?;
        if target.exists() && matches_hash(&target, &entry.hash)? {
            continue;
        }
        let source = safe_path(&workspace, Path::new(&entry.path))?;
        let staged = safe_path(&directory, &Path::new("new").join(&entry.path))?;
        fs::create_dir_all(staged.parent().unwrap())?;
        let mut input = File::open(&source)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        drop(output);
        ensure!(
            matches_hash(&staged, &entry.hash)?,
            "下载的模组不完整：{}",
            entry.path
        );
    }
    let transaction = Transaction {
        old,
        next,
        directory: directory_name,
    };
    write_owned(&journal, &serde_json::to_vec_pretty(&transaction)?)?;
    finish_transaction(base_dir, &transaction)?;
    if first_install {
        for entry in downloaded["cachedFiles"].as_object().unwrap().values() {
            let Some(location) = entry["cachedLocation"].as_str() else {
                continue;
            };
            if mod_path(location).is_ok() {
                continue;
            }
            let relative = Path::new(location);
            let source = safe_path(&workspace, relative)?;
            create_default(&game, relative, &fs::read(source)?)?;
        }
    }
    Ok(())
}
