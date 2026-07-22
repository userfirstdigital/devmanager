use devmanager::updater::{
    github_release_manifest_endpoint, is_remote_version_newer, next_patch_release_version,
    parse_release_manifest, resolve_updater_config, UpdaterWindowsInstallMode,
};
use std::fs;
use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn release_workflow_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".github")
        .join("workflows")
        .join("release.yml")
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
fn next_patch_release_version_uses_latest_release_when_available() {
    assert_eq!(
        next_patch_release_version(Some("v0.2.4"), "0.2.0").expect("next version"),
        "0.2.5"
    );
}

#[test]
fn next_patch_release_version_falls_back_to_cargo_version_without_tags() {
    assert_eq!(
        next_patch_release_version(None, "0.2.0").expect("next version"),
        "0.2.1"
    );
}

#[test]
fn github_release_endpoint_matches_workflow_location() {
    assert_eq!(
        github_release_manifest_endpoint("example/devmanager"),
        "https://github.com/example/devmanager/releases/latest/download/latest.json"
    );
}

#[test]
fn release_verify_installs_rustfmt_before_running_cargo_fmt() {
    let workflow = fs::read_to_string(release_workflow_path()).expect("read release workflow");
    let verify_job = workflow
        .split("\n  prepare:")
        .next()
        .expect("verify job should precede prepare");
    let rust_install = verify_job
        .split("- name: Install Rust stable")
        .nth(1)
        .and_then(|tail| tail.split("\n      - name:").next())
        .expect("verify job should install Rust");

    assert!(verify_job.contains("cargo fmt --all -- --check"));
    assert!(
        rust_install.contains("components: rustfmt"),
        "the minimal Rust toolchain must install cargo-fmt before verification"
    );
}

#[test]
fn release_packaging_runs_independently_of_verify_but_publish_requires_verify() {
    let workflow = fs::read_to_string(release_workflow_path()).expect("read release workflow");
    let prepare_job = workflow
        .split("\n  prepare:")
        .nth(1)
        .and_then(|tail| tail.split("\n  build:").next())
        .expect("prepare job should precede build");
    let build_job = workflow
        .split("\n  build:")
        .nth(1)
        .and_then(|tail| tail.split("\n  release:").next())
        .expect("build job should precede release");
    let release_job = workflow
        .split("\n  release:")
        .nth(1)
        .expect("release job should exist");
    let release_header = release_job
        .split("\n    steps:")
        .next()
        .expect("release job should declare steps");

    assert!(
        !prepare_job.contains("needs: verify") && !prepare_job.contains("needs: [verify"),
        "prepare must not wait on verify so packaging still runs when verification fails"
    );
    assert!(
        !build_job.contains("needs: verify") && !build_job.contains("needs: [verify"),
        "build must not wait on verify so packaging still runs when verification fails"
    );
    assert!(
        build_job.contains("needs: prepare") || build_job.contains("needs: [prepare"),
        "build must still wait for version preparation"
    );
    assert!(
        release_header.contains("needs: [verify, prepare, build]")
            || release_header.contains("needs: [prepare, build, verify]")
            || release_header.contains("needs: [build, verify, prepare]")
            || release_header.contains("needs: [verify, build, prepare]")
            || release_header.contains("needs: [prepare, verify, build]")
            || release_header.contains("needs: [build, prepare, verify]"),
        "release publication must require verify, prepare, and build together"
    );
    assert!(
        build_job.contains("platform: windows-x86_64")
            && build_job.contains("formats: nsis,wix")
            && release_job.contains("\"*_x64-setup.exe\"")
            && release_job.contains("\"*.msi\""),
        "Windows x64 NSIS EXE and WiX MSI assets must remain mandatory in packaging and publish"
    );
}

#[test]
fn release_build_reuses_the_tracked_fingerprinted_web_bundle() {
    let workflow = fs::read_to_string(release_workflow_path()).expect("read release workflow");
    let build_job = workflow
        .split("\n  build:")
        .nth(1)
        .and_then(|tail| tail.split("\n  release:").next())
        .expect("build job should precede release");

    assert!(build_job.contains("cargo test remote::web::assets --lib"));
    assert!(
        !build_job.contains("npm --prefix web") && !build_job.contains("rm -rf web/bundle"),
        "platform packaging must reuse the tracked fingerprinted bundle and must not rebuild it"
    );
}

#[test]
fn release_windows_build_exports_the_installed_nsis_directory() {
    let workflow = fs::read_to_string(release_workflow_path()).expect("read release workflow");
    let build_job = workflow
        .split("\n  build:")
        .nth(1)
        .and_then(|tail| tail.split("\n  release:").next())
        .expect("build job should precede release");
    let nsis_install = build_job
        .split("- name: Install NSIS")
        .nth(1)
        .and_then(|tail| tail.split("\n      - name:").next())
        .expect("Windows build should install NSIS");

    assert!(nsis_install.contains("Join-Path ${env:ProgramFiles(x86)} \"NSIS\""));
    assert!(nsis_install.contains("& $makensis /VERSION"));
    assert!(
        nsis_install.contains("$env:GITHUB_PATH"),
        "the package step needs the newly installed NSIS directory on PATH"
    );
}

#[test]
fn release_draft_id_is_resolved_from_the_authenticated_release_list() {
    let workflow = fs::read_to_string(release_workflow_path()).expect("read release workflow");
    let draft_step = workflow
        .split("- name: Create draft release and upload assets")
        .nth(1)
        .and_then(|tail| tail.split("\n      - name:").next())
        .expect("release job should create a draft release");

    assert!(draft_step.contains("repos/${REPO}/releases?per_page=100"));
    assert!(draft_step.contains(".draft == true"));
    assert!(
        draft_step.contains("for attempt in $(seq 1 12)"),
        "GitHub can briefly omit a newly created draft from the authenticated release list"
    );
    assert!(draft_step.contains("sleep 5"));
    assert!(
        !draft_step.contains("releases/tags/${TAG_NAME}"),
        "GitHub's release-by-tag endpoint does not expose an unpublished draft"
    );
}

#[test]
fn manifest_fixture_parses_expected_platform_assets() {
    let manifest_text = fs::read_to_string(fixture_path("latest.json")).expect("manifest fixture");
    let manifest = parse_release_manifest(&manifest_text).expect("parse manifest fixture");

    assert_eq!(manifest.version, "0.2.2");
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
        .get("macos-aarch64")
        .expect("mac updater entry");
    assert_eq!(mac.format, "app");
    assert!(mac.url.ends_with(".app.tar.gz"));
    assert_eq!(mac.signature, "mac-signature-placeholder");
}

#[test]
fn version_compare_accepts_prefixed_manifest_versions() {
    assert!(is_remote_version_newer("0.2.2", "v0.2.3").expect("compare versions"));
    assert!(!is_remote_version_newer("0.2.3", "0.2.3").expect("compare equal versions"));
}
