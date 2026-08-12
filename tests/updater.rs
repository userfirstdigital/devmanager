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
fn release_packaging_runs_independently_of_verify_but_stage_requires_verify() {
    let workflow = fs::read_to_string(release_workflow_path()).expect("read release workflow");
    let prepare_job = workflow
        .split("\n  prepare:")
        .nth(1)
        .and_then(|tail| tail.split("\n  build:").next())
        .expect("prepare job should precede build");
    let build_job = workflow
        .split("\n  build:")
        .nth(1)
        .and_then(|tail| tail.split("\n  stage:").next())
        .expect("build job should precede stage");
    let stage_job = workflow
        .split("\n  stage:")
        .nth(1)
        .and_then(|tail| tail.split("\n  publish:").next())
        .expect("stage job should precede publish");
    let stage_header = stage_job
        .split("\n    steps:")
        .next()
        .expect("stage job should declare steps");
    let publish_job = workflow
        .split("\n  publish:")
        .nth(1)
        .expect("publish job should exist");
    let publish_header = publish_job
        .split("\n    steps:")
        .next()
        .expect("publish job should declare steps");

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
        stage_header.contains("needs: [verify, prepare, build]")
            || stage_header.contains("needs: [prepare, build, verify]")
            || stage_header.contains("needs: [build, verify, prepare]")
            || stage_header.contains("needs: [verify, build, prepare]")
            || stage_header.contains("needs: [prepare, verify, build]")
            || stage_header.contains("needs: [build, prepare, verify]"),
        "stage must require verify, prepare, and build together"
    );
    assert!(
        !publish_header.contains("needs:"),
        "publish must be an independent protected job that does not need stage/prepare outputs"
    );
    assert!(
        publish_job.contains("inputs.tag_name")
            || publish_job.contains("TAG_NAME: ${{ inputs.tag_name }}")
            || publish_job.contains("TAG_NAME: ${{ steps.selected.outputs.tag_name }}"),
        "publish must promote an explicitly selected existing draft tag input"
    );
    assert!(
        publish_job.contains("recompute") || publish_job.contains("recomputed"),
        "publish must recompute artifact hashes and compare latest.json"
    );
    assert!(
        build_job.contains("platform: windows-x86_64")
            && build_job.contains("formats: nsis,wix")
            && stage_job.contains("\"*_x64-setup.exe\"")
            && stage_job.contains("\"*.msi\"")
            && stage_job.contains("\"*_x64-update.zip\"")
            && build_job.contains("Build signed dual-binary updater payload"),
        "Windows x64 must keep NSIS/MSI for manual install and publish a dual-binary updater ZIP"
    );
    assert!(
        stage_job.contains("publish-assets-expected-names.txt"),
        "stage must declare the exact emitted publish-assets name set for draft verification"
    );
    assert!(
        !stage_job.contains("Expected exactly 11 non-empty staged release assets"),
        "draft asset verification must not keep the stale 11-asset count"
    );
}

#[test]
fn release_build_reuses_the_tracked_fingerprinted_web_bundle() {
    let workflow = fs::read_to_string(release_workflow_path()).expect("read release workflow");
    let build_job = workflow
        .split("\n  build:")
        .nth(1)
        .and_then(|tail| tail.split("\n  stage:").next())
        .expect("build job should precede stage");

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
        .and_then(|tail| tail.split("\n  stage:").next())
        .expect("build job should precede stage");
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
        .expect("stage job should create a draft release");

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
fn release_latest_json_includes_protocol_hash_and_build_identity() {
    let workflow = fs::read_to_string(release_workflow_path()).expect("read release workflow");
    // Staging owns manifest generation; public publication is a separate
    // protected job and must not be required for the draft contract.
    let release_job = workflow
        .split("\n  stage:")
        .nth(1)
        .and_then(|tail| tail.split("\n  publish:").next())
        .expect("stage job");
    assert!(release_job.contains("\"minimum_protocol\": \"1.0\""));
    assert!(release_job.contains("\"hash\": f\"sha256:{digest}\""));
    assert!(release_job.contains("\"client_build\": f\"devmanager/{version}\""));
    assert!(release_job.contains("\"host_build\": f\"devmanager-host/{version}\""));
    assert!(release_job.contains("\"*_x64-update.zip\""));
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
    assert!(windows.url.ends_with("_x64-update.zip") || windows.url.ends_with("_x64-setup.exe"));
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
