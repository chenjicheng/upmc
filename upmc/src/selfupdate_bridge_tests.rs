use super::*;

const LOCAL: &str = "1111111111111111111111111111111111111111";
const REMOTE: &str = "2222222222222222222222222222222222222222";

fn manifest(version: &str, build: &str) -> serde_json::Value {
    serde_json::json!({
        "version": version,
        "build_id": build,
        "download_url": "https://github.com/chenjicheng/upmc/releases/download/v0.4.8/updater.exe",
        "sha256": "a".repeat(64),
        "size": 100
    })
}

fn required(version: &str, build: &str, local: Option<&str>, channel: UpdateChannel) -> bool {
    let info: UpdaterVersionInfo = serde_json::from_value(manifest(version, build)).unwrap();
    bridge_update_required(&info, "0.4.8", local, channel).unwrap()
}

#[test]
fn bridge_uses_separate_entrypoints_for_both_channels() {
    assert_eq!(
        config::updater_version_url(UpdateChannel::Stable),
        "https://upmc.chenjicheng.cn/bridge/version.json"
    );
    assert_eq!(
        config::updater_version_url(UpdateChannel::Dev),
        "https://upmc.chenjicheng.cn/bridge/dev/version.json"
    );
}

#[test]
fn bridge_stable_requires_strictly_higher_semver() {
    assert!(!required(
        "0.4.8",
        REMOTE,
        Some(LOCAL),
        UpdateChannel::Stable
    ));
    assert!(!required(
        "0.4.7",
        REMOTE,
        Some(LOCAL),
        UpdateChannel::Stable
    ));
    assert!(!required(
        "0.4.8-rc.1",
        REMOTE,
        Some(LOCAL),
        UpdateChannel::Stable
    ));
    assert!(!required(
        "0.4.8+rebuild",
        REMOTE,
        Some(LOCAL),
        UpdateChannel::Stable
    ));
    assert!(required(
        "0.4.9",
        REMOTE,
        Some(LOCAL),
        UpdateChannel::Stable
    ));
    assert!(required(
        "0.6.0",
        REMOTE,
        Some(LOCAL),
        UpdateChannel::Stable
    ));
}

#[test]
fn bridge_dev_allows_explicit_same_version_build_but_never_downgrades() {
    assert!(!required("0.4.7", REMOTE, Some(LOCAL), UpdateChannel::Dev));
    assert!(required("0.4.8", REMOTE, Some(LOCAL), UpdateChannel::Dev));
    assert!(!required("0.4.8", LOCAL, Some(LOCAL), UpdateChannel::Dev));
    assert!(!required("0.4.8", REMOTE, None, UpdateChannel::Dev));
    assert!(required("0.6.0", REMOTE, None, UpdateChannel::Dev));
}

#[test]
fn bridge_same_build_and_version_is_no_update() {
    for channel in [UpdateChannel::Stable, UpdateChannel::Dev] {
        assert!(!required("0.4.8", LOCAL, Some(LOCAL), channel));
    }
}

#[test]
fn bridge_missing_or_null_metadata_fails_before_update_selection() {
    for field in ["version", "build_id", "download_url", "sha256", "size"] {
        let mut value = manifest("0.4.8", LOCAL);
        value.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<UpdaterVersionInfo>(value).is_err(),
            "missing {field} accepted"
        );
        let mut value = manifest("0.4.8", LOCAL);
        value[field] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<UpdaterVersionInfo>(value).is_err(),
            "null {field} accepted"
        );
    }
}

#[test]
fn bridge_malformed_version_build_hash_size_and_url_fail_closed() {
    for (field, bad) in [
        ("version", serde_json::json!("v0.4.8")),
        ("version", serde_json::json!("0.4")),
        ("build_id", serde_json::json!("abc1234")),
        ("build_id", serde_json::json!("g".repeat(40))),
        ("sha256", serde_json::json!("")),
        ("sha256", serde_json::json!("g".repeat(64))),
        ("sha256", serde_json::json!("a".repeat(63))),
        ("size", serde_json::json!(0)),
        ("size", serde_json::json!(-1)),
        ("size", serde_json::json!(1.5)),
        (
            "download_url",
            serde_json::json!(
                "http://github.com/chenjicheng/upmc/releases/download/v0.4.8/updater.exe"
            ),
        ),
        (
            "download_url",
            serde_json::json!(
                "https://github.com.evil.test/chenjicheng/upmc/releases/download/v0.4.8/updater.exe"
            ),
        ),
        (
            "download_url",
            serde_json::json!(
                "https://github.com/another/upmc/releases/download/v0.4.8/updater.exe"
            ),
        ),
        (
            "download_url",
            serde_json::json!(
                "https://github.com/chenjicheng/upmc/releases/download/v0.4.8/evil.exe"
            ),
        ),
        (
            "download_url",
            serde_json::json!(
                "https://github.com/chenjicheng/upmc/releases/download/v0.4.8/updater.exe?token=x"
            ),
        ),
        (
            "download_url",
            serde_json::json!(
                "https://user@github.com/chenjicheng/upmc/releases/download/v0.4.8/updater.exe"
            ),
        ),
    ] {
        let mut value = manifest("0.4.8", LOCAL);
        value[field] = bad.clone();
        assert!(
            serde_json::from_value::<UpdaterVersionInfo>(value).is_err(),
            "accepted {field}: {bad}"
        );
    }
}

#[test]
fn bridge_official_release_and_existing_proxy_are_accepted() {
    for url in [
        "https://github.com/chenjicheng/upmc/releases/download/v0.4.8/updater.exe",
        "https://github.com/chenjicheng/upmc/releases/download/dev-latest/updater.exe",
        "https://gh.chenjicheng.cn/https://github.com/chenjicheng/upmc/releases/download/v0.4.8/updater.exe",
    ] {
        let mut value = manifest("0.4.8", LOCAL);
        value["download_url"] = serde_json::json!(url);
        assert!(serde_json::from_value::<UpdaterVersionInfo>(value).is_ok());
    }
}

#[test]
fn bridge_manifest_remains_readable_by_old_discovery_reader() {
    #[derive(Deserialize)]
    struct OldReader {
        download_url: String,
        build_id: Option<String>,
        sha256: Option<String>,
    }
    let info: OldReader = serde_json::from_value(manifest("0.4.8", REMOTE)).unwrap();
    assert_ne!(info.build_id.as_deref(), Some(LOCAL));
    assert!(info.download_url.ends_with("/updater.exe"));
    assert_eq!(info.sha256.as_deref(), Some("a".repeat(64).as_str()));
}
