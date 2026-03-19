use devmanager::updater::{
    is_remote_version_newer, parse_release_manifest, resolve_updater_config,
    UpdaterWindowsInstallMode,
};
use std::fs;
use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn updater_config_requires_endpoint_and_pubkey_together() {
    assert!(resolve_updater_config(
        Some("https://github.com/example/devmanager/releases/latest/download/latest.json".into()),
        None,
        None,
    )
    .is_err());

    assert!(resolve_updater_config(None, Some("public-key".into()), None).is_err());
    assert!(resolve_updater_config(None, None, None)
        .expect("missing config is allowed")
        .is_none());
}

#[test]
fn updater_config_parses_multiple_endpoints_and_install_mode() {
    let resolved = resolve_updater_config(
        Some(
            "https://github.com/example/devmanager/releases/latest/download/latest.json,\nhttps://mirror.example.com/devmanager/latest.json".into(),
        ),
        Some("public-key".into()),
        Some("quiet".into()),
    )
    .expect("valid updater config")
    .expect("configured updater");

    assert_eq!(
        resolved.endpoints,
        vec![
            "https://github.com/example/devmanager/releases/latest/download/latest.json"
                .to_string(),
            "https://mirror.example.com/devmanager/latest.json".to_string(),
        ]
    );
    assert_eq!(resolved.pubkey, "public-key");
    assert_eq!(
        resolved.windows_install_mode,
        UpdaterWindowsInstallMode::Quiet
    );
}

#[test]
fn manifest_fixture_parses_expected_platform_assets() {
    let manifest_text = fs::read_to_string(fixture_path("latest.json")).expect("manifest fixture");
    let manifest = parse_release_manifest(&manifest_text).expect("parse manifest fixture");

    assert_eq!(manifest.version, "0.2.0-dev.42");
    assert_eq!(
        manifest.notes.as_deref(),
        Some("Release notes live on GitHub.")
    );

    let windows = manifest
        .platforms
        .get("windows-x86_64")
        .expect("windows updater entry");
    assert_eq!(windows.format, "nsis");
    assert!(windows.url.ends_with("_x64-setup.exe"));
    assert_eq!(windows.signature, "windows-signature-placeholder");

    let mac = manifest
        .platforms
        .get("darwin-aarch64")
        .expect("mac updater entry");
    assert_eq!(mac.format, "app");
    assert!(mac.url.ends_with(".app.tar.gz"));
    assert_eq!(mac.signature, "mac-signature-placeholder");
}

#[test]
fn version_compare_accepts_prefixed_manifest_versions() {
    assert!(is_remote_version_newer("0.2.0-dev.1", "v0.2.0-dev.2").expect("compare versions"));
    assert!(!is_remote_version_newer("0.2.0-dev.2", "0.2.0-dev.2").expect("compare equal versions"));
}
