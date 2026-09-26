//! Packwiz downloads into updater-owned storage. Only explicitly tracked JARs
//! may be reconciled into the game directory; player data never goes to Java.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use std::collections::{BTreeMap, BTreeSet};
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
fn safe_path(root: &Path, relative: &Path) -> Result<PathBuf> {
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
            "为保护玩家文件，更新不跟随链接或目录联接：{}", path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("读取文件信息失败：{}", path.display())),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(bytes: &[u8]) -> FileHash {
        FileHash { kind: "sha256".into(), value: format!("{:x}", Sha256::digest(bytes)) }
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
            cached.insert(id.to_string(), serde_json::json!({
                "cachedLocation": path, "linkedFileHash": hash(content)
            }));
        }
        write(&workspace, "packwiz.json", &serde_json::to_vec(&serde_json::json!({
            "cachedFiles": cached, "packFileHash": hash(b"pack"), "indexFileHash": hash(b"index")
        })).unwrap());
    }

    fn tracked(base: &Path, files: &[(&str, &str, &[u8])]) {
        let files = files.iter().map(|(id, path, content)| (
            id.to_string(), ModFile { path: path.to_string(), hash: hash(content) }
        )).collect();
        write(base, MANIFEST, &serde_json::to_vec(&Manifest { files }).unwrap());
    }

    #[test]
    fn updates_managed_mods_without_touching_player_content() {
        let root = fixture();
        let base = root.path();
        write(base, ".minecraft/mods/old.jar", b"old managed");
        write(base, ".minecraft/mods/my-extra.jar", b"player mod");
        let player_files = ["config/litematica.json", "options.txt", "servers.dat",
            "saves/My world/level.dat", "resourcepacks/custom.zip", "screenshots/shot.png",
            "versions/old/PCL/Setup.ini", "versions/old/config/tweakeroo.json"];
        for path in player_files { write(&base.join(".minecraft"), path, b"player data"); }
        tracked(base, &[("mods/managed.pw.toml", "mods/old.jar", b"old managed")]);
        downloaded(base, &[("mods/managed.pw.toml", "mods/new.jar", b"new managed"),
            ("config/litematica.json", "config/litematica.json", b"server defaults"),
            ("options.txt", "options.txt", b"server keybindings")]);
        apply(base, false).unwrap();
        assert_eq!(fs::read(base.join(".minecraft/mods/new.jar")).unwrap(), b"new managed");
        assert!(!base.join(".minecraft/mods/old.jar").exists());
        assert_eq!(fs::read(base.join(".minecraft/mods/my-extra.jar")).unwrap(), b"player mod");
        for path in player_files {
            assert_eq!(fs::read(base.join(".minecraft").join(path)).unwrap(), b"player data", "{path}");
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
        downloaded(root.path(), &[("options.txt", "options.txt", b"defaults"),
            ("config/new.json", "config/new.json", b"first defaults")]);
        apply(root.path(), true).unwrap();
        assert_eq!(fs::read(root.path().join(".minecraft/options.txt")).unwrap(), b"player keys");
        assert_eq!(fs::read(root.path().join(".minecraft/config/new.json")).unwrap(), b"first defaults");
    }

    #[test]
    fn same_name_player_jar_conflict_prevents_all_game_writes() {
        let root = fixture();
        write(root.path(), ".minecraft/mods/a.jar", b"old");
        write(root.path(), ".minecraft/mods/z.jar", b"player extra");
        tracked(root.path(), &[("a", "mods/a.jar", b"old")]);
        downloaded(root.path(), &[("a", "mods/a.jar", b"new"), ("z", "mods/z.jar", b"server")]);
        assert!(apply(root.path(), false).is_err());
        assert_eq!(fs::read(root.path().join(".minecraft/mods/a.jar")).unwrap(), b"old");
        assert_eq!(fs::read(root.path().join(".minecraft/mods/z.jar")).unwrap(), b"player extra");
    }

    #[test]
    fn changed_managed_mod_is_preserved_when_remote_removes_it() {
        let root = fixture();
        tracked(root.path(), &[("a", "mods/a.jar", b"original")]);
        write(root.path(), ".minecraft/mods/a.jar", b"player modified");
        downloaded(root.path(), &[]);
        assert!(apply(root.path(), false).is_err());
        assert_eq!(fs::read(root.path().join(".minecraft/mods/a.jar")).unwrap(), b"player modified");
    }

    #[test]
    fn deleting_markers_does_not_make_an_existing_game_a_new_install() {
        let root = tempfile::tempdir().unwrap();
        assert!(crate::bootstrap::begin_first_install(root.path()).unwrap());
        assert!(!crate::bootstrap::begin_first_install(root.path()).unwrap());
        write(root.path(), ".minecraft/config/keys.json", b"player keys");
        fs::remove_file(root.path().join("updater/.initial_install_started")).unwrap();
        assert!(!crate::bootstrap::begin_first_install(root.path()).unwrap());
        assert_eq!(fs::read(root.path().join(".minecraft/config/keys.json")).unwrap(), b"player keys");
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
        assert_eq!(fs::read(root.path().join(".minecraft/mods/a.jar")).unwrap(), b"old");
    }
}

fn mod_path(value: &str) -> Result<String> {
    let value = value.replace('\\', "/");
    let pieces: Vec<_> = value.split('/').collect();
    ensure!(pieces.len() == 2 && pieces[0] == "mods"
        && pieces[1].to_ascii_lowercase().ends_with(".jar"),
        "受管理内容必须是 mods/ 中的 JAR：{value}");
    // safe_path performs the remaining platform-specific checks at use time.
    Ok(value)
}

fn matches_hash(path: &Path, expected: &FileHash) -> Result<bool> {
    let mut file = File::open(path)?;
    ensure!(file.metadata()?.is_file(), "不是普通文件：{}", path.display());
    let mut buf = [0u8; 65536];
    let actual = match expected.kind.as_str() {
        "sha256" => {
            let mut hash = Sha256::new();
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 { break; }
                hash.update(&buf[..n]);
            }
            format!("{:x}", hash.finalize())
        }
        "sha512" => {
            let mut hash = Sha512::new();
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 { break; }
                hash.update(&buf[..n]);
            }
            format!("{:x}", hash.finalize())
        }
        _ => bail!("无法确认旧模组摘要 {}，已保留玩家文件：{}", expected.kind, path.display()),
    };
    Ok(actual.eq_ignore_ascii_case(&expected.value))
}

fn read_packwiz(path: &Path) -> Result<serde_json::Value> {
    serde_json::from_slice(&fs::read(path)?).context("读取 Packwiz 安装记录失败")
}

fn records(value: &serde_json::Value) -> Result<Manifest> {
    let entries = value["cachedFiles"].as_object().context("Packwiz 安装记录缺少 cachedFiles")?;
    let mut files = BTreeMap::new();
    for (id, entry) in entries {
        let Some(location) = entry["cachedLocation"].as_str() else { continue; };
        let Ok(path) = mod_path(location) else { continue; };
        let hash = entry.get("linkedFileHash").filter(|hash| !hash.is_null())
            .or_else(|| entry.get("hash")).context("模组缺少下载摘要")?;
        files.insert(id.clone(), ModFile {
            path,
            hash: serde_json::from_value(hash.clone()).context("模组摘要格式错误")?,
        });
    }
    Ok(Manifest { files })
}

fn previous(base_dir: &Path) -> Result<Manifest> {
    let path = base_dir.join(MANIFEST);
    if path.exists() {
        let mut manifest: Manifest = serde_json::from_slice(&fs::read(path)?)
            .context("受管理模组记录损坏，已停止更新以保护玩家文件")?;
        for file in manifest.files.values_mut() {
            file.path = mod_path(&file.path)?;
        }
        return Ok(manifest);
    }
    let legacy = base_dir.join(config::MINECRAFT_DIR).join("packwiz.json");
    if legacy.is_file() { records(&read_packwiz(&legacy)?) } else { Ok(Manifest::default()) }
}

/// Atomic create-only defaults: a missing marker never authorizes replacement.
pub(crate) fn create_default(root: &Path, relative: &Path, bytes: &[u8]) -> Result<()> {
    let path = safe_path(root, relative)?;
    if path.exists() { return Ok(()); }
    fs::create_dir_all(path.parent().context("默认文件没有父目录")?)?;
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("创建默认文件失败：{}", path.display())),
    }
    Ok(())
}

pub(crate) fn prepare_workspace(base_dir: &Path) -> Result<PathBuf> {
    let workspace = safe_path(base_dir, Path::new(WORKSPACE))?;
    fs::create_dir_all(&workspace)?;
    Ok(workspace)
}

pub(crate) fn apply(base_dir: &Path, first_install: bool) -> Result<()> {
    let workspace = safe_path(base_dir, Path::new(WORKSPACE))?;
    let game = safe_path(base_dir, Path::new(config::MINECRAFT_DIR))?;
    let downloaded = read_packwiz(&workspace.join("packwiz.json"))?;
    ensure!(downloaded.get("packFileHash").is_some_and(|v| !v.is_null())
        && downloaded.get("indexFileHash").is_some_and(|v| !v.is_null()),
        "Packwiz 下载尚未完整成功，玩家目录保持不变");
    let next = records(&downloaded)?;
    let old = previous(base_dir)?;
    let mut destinations = BTreeSet::new();

    // Resolve ALL conflicts before changing any game file. Local modifications
    // and unknown same-name JARs require explicit player intervention.
    for (id, entry) in &next.files {
        let path = mod_path(&entry.path)?;
        ensure!(destinations.insert(path.to_ascii_lowercase()), "多个模组使用同一文件名：{path}");
        let source = safe_path(&workspace, Path::new(&path))?;
        ensure!(matches_hash(&source, &entry.hash)?, "下载的模组不完整：{path}");
        let target = safe_path(&game, Path::new(&path))?;
        if let Some(prior) = old.files.get(id) {
            let prior_path = mod_path(&prior.path)?;
            let prior_target = safe_path(&game, Path::new(&prior_path))?;
            if prior_target.exists() && !matches_hash(&prior_target, &prior.hash)? {
                ensure!(prior_path.eq_ignore_ascii_case(&path) && matches_hash(&prior_target, &entry.hash)?,
                    "玩家修改过模组，已保留并停止更新：{prior_path}");
            }
        }
        if target.exists() && !matches_hash(&target, &entry.hash)? {
            let owner = old.files.values().find(|prior| prior.path.eq_ignore_ascii_case(&path));
            ensure!(owner.is_some_and(|prior| matches_hash(&target, &prior.hash).unwrap_or(false)),
                "发现玩家已有的同名文件，更新不会覆盖：{path}");
        }
    }
    for prior in old.files.values() {
        let path = mod_path(&prior.path)?;
        let target = safe_path(&game, Path::new(&path))?;
        if !destinations.contains(&path.to_ascii_lowercase()) && target.exists() {
            ensure!(matches_hash(&target, &prior.hash)?, "玩家修改过旧模组，已保留并停止更新：{path}");
        }
    }

    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
    let backup = safe_path(base_dir, Path::new(&format!("updater/mod-backups/{nonce}")))?;
    for entry in next.files.values() {
        let source = safe_path(&workspace, Path::new(&entry.path))?;
        let target = safe_path(&game, Path::new(&entry.path))?;
        if target.exists() && matches_hash(&target, &entry.hash)? { continue; }
        let backup_path = backup.join(&entry.path);
        let replaced = target.exists();
        if replaced {
            let prior = old.files.values().find(|prior| prior.path.eq_ignore_ascii_case(&entry.path))
                .context("目标文件在下载期间被创建，已保留玩家文件")?;
            ensure!(matches_hash(&target, &prior.hash)?, "模组在更新期间被修改，已保留：{}", entry.path);
            fs::create_dir_all(backup_path.parent().unwrap())?;
            fs::rename(&target, &backup_path).context("无法备份受管理模组，请关闭游戏后重试")?;
        }
        let install = (|| -> Result<()> {
            fs::create_dir_all(target.parent().unwrap())?;
            let mut output = OpenOptions::new().write(true).create_new(true).open(&target)?;
            let copied = std::io::copy(&mut File::open(&source)?, &mut output);
            if let Err(error) = copied {
                drop(output);
                let _ = fs::remove_file(&target);
                return Err(error).context("复制受管理模组失败");
            }
            output.sync_all()?;
            Ok(())
        })();
        if let Err(error) = install {
            if replaced && !target.exists() {
                let _ = fs::rename(&backup_path, &target);
            }
            return Err(error).with_context(|| format!("更新模组失败；原文件保存在 {}", backup_path.display()));
        }
    }
    // Retire only tracked and unchanged old JARs, retaining a recoverable copy.
    for prior in old.files.values() {
        if destinations.contains(&prior.path.to_ascii_lowercase()) { continue; }
        let target = safe_path(&game, Path::new(&prior.path))?;
        if target.exists() {
            ensure!(matches_hash(&target, &prior.hash)?, "旧模组在更新过程中被修改，已保留：{}", prior.path);
            let saved = backup.join(&prior.path);
            fs::create_dir_all(saved.parent().unwrap())?;
            fs::rename(&target, &saved)?;
        }
    }
    if first_install {
        for entry in downloaded["cachedFiles"].as_object().unwrap().values() {
            let Some(location) = entry["cachedLocation"].as_str() else { continue; };
            if mod_path(location).is_ok() { continue; }
            let relative = Path::new(location);
            let source = safe_path(&workspace, relative)?;
            create_default(&game, relative, &fs::read(source)?)?;
        }
    }
    let path = safe_path(base_dir, Path::new(MANIFEST))?;
    let temporary = path.with_extension(format!("{nonce}.pending"));
    let mut output = OpenOptions::new().write(true).create_new(true).open(&temporary)?;
    output.write_all(&serde_json::to_vec_pretty(&next)?)?;
    output.sync_all()?;
    drop(output);
    fs::rename(&temporary, &path).context("保存受管理模组记录失败")?;
    Ok(())
}
