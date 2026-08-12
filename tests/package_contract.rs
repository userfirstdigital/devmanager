//! Package cutover contract: sibling client/host identity without legacy binaries.
//!
//! These checks are deterministic source/fixture inspections. They do not build,
//! install, or execute packaged binaries.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&read(path))
        .unwrap_or_else(|error| panic!("parse json {}: {error}", path.display()))
}

fn package_contract() -> Value {
    read_json(&repo_root().join("packaging/package-contract.json"))
}

fn cargo_toml() -> String {
    read(&repo_root().join("Cargo.toml"))
}

fn cargo_package_version(cargo_toml: &str) -> &str {
    let mut in_package = false;
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if trimmed == "[package]" {
            in_package = true;
            continue;
        }
        if in_package && trimmed.starts_with('[') && trimmed != "[package]" {
            break;
        }
        if in_package {
            if let Some(version) = trimmed
                .strip_prefix("version = \"")
                .and_then(|value| value.strip_suffix('"'))
            {
                return version;
            }
        }
    }
    panic!("Cargo.toml package.version missing");
}

fn assert_packager_binary(cargo_toml: &str, name: &str, main: bool) {
    let main_literal = if main { "true" } else { "false" };
    let needle =
        format!("[[package.metadata.packager.binaries]]\npath = \"{name}\"\nmain = {main_literal}");
    assert!(
        cargo_toml.contains(&needle),
        "Cargo.toml must declare packager binary {name} with main={main_literal}"
    );
}

fn validate_manifest(manifest: &Value, contract: &Value, version: &str) -> Result<(), String> {
    let product = contract["productName"]
        .as_str()
        .ok_or("contract productName")?;
    if manifest["productName"].as_str() != Some(product) {
        return Err(format!(
            "manifest productName {:?} != contract {product}",
            manifest["productName"]
        ));
    }
    if manifest["version"].as_str() != Some(version) {
        return Err(format!(
            "manifest version {:?} != cargo {version}",
            manifest["version"]
        ));
    }

    let expected_identity = format!("devmanager/{version}");
    if manifest["identity"].as_str() != Some(expected_identity.as_str()) {
        return Err(format!(
            "manifest identity {:?} != {expected_identity}",
            manifest["identity"]
        ));
    }

    let protocol = &contract["protocol"];
    if manifest["protocol"]["major"] != protocol["major"]
        || manifest["protocol"]["minor"] != protocol["minor"]
    {
        return Err(format!(
            "manifest protocol {:?} != contract {:?}",
            manifest["protocol"], protocol
        ));
    }

    let binaries = manifest["binaries"]
        .as_array()
        .ok_or("manifest.binaries must be an array")?;
    let expected = contract["binaries"]
        .as_array()
        .ok_or("contract.binaries must be an array")?;
    if binaries.len() != expected.len() {
        return Err(format!(
            "manifest has {} binaries, contract requires {}",
            binaries.len(),
            expected.len()
        ));
    }

    let forbidden = contract["forbiddenBinaries"]
        .as_array()
        .ok_or("contract.forbiddenBinaries")?;
    for binary in binaries {
        let name = binary["name"].as_str().unwrap_or_default();
        for entry in forbidden {
            if entry.as_str() == Some(name) {
                return Err(format!("forbidden binary present in manifest: {name}"));
            }
        }
        if binary["fileVersion"].as_str() != Some(version)
            || binary["productVersion"].as_str() != Some(version)
        {
            return Err(format!(
                "binary {name} version metadata must match semantic version {version}"
            ));
        }
        if binary["productName"].as_str() != Some(product) {
            return Err(format!("binary {name} productName must be {product}"));
        }
        let expected_build = if name == "devmanager" {
            format!("devmanager/{version}")
        } else if name == "devmanager-host" {
            format!("devmanager-host/{version}")
        } else {
            String::new()
        };
        if !expected_build.is_empty()
            && binary["buildIdentity"].as_str() != Some(expected_build.as_str())
        {
            return Err(format!(
                "binary {name} buildIdentity {:?} != {expected_build}",
                binary["buildIdentity"]
            ));
        }
    }

    for expected_binary in expected {
        let name = expected_binary["name"].as_str().unwrap_or_default();
        let role = expected_binary["role"].as_str().unwrap_or_default();
        let windows = &expected_binary["windows"];
        let found = binaries
            .iter()
            .find(|binary| binary["name"].as_str() == Some(name));
        let Some(found) = found else {
            return Err(format!("missing required binary {name}"));
        };
        if found["role"].as_str() != Some(role) {
            return Err(format!("binary {name} role mismatch"));
        }
        if found["fileDescription"].as_str() != windows["fileDescription"].as_str() {
            return Err(format!("binary {name} fileDescription mismatch"));
        }
        if found["originalFilename"].as_str() != windows["originalFilename"].as_str() {
            return Err(format!("binary {name} originalFilename mismatch"));
        }
        if found["internalName"].as_str() != windows["internalName"].as_str() {
            return Err(format!("binary {name} internalName mismatch"));
        }
    }

    let names: Vec<&str> = binaries
        .iter()
        .filter_map(|binary| binary["name"].as_str())
        .collect();
    if !(names.contains(&"devmanager") && names.contains(&"devmanager-host")) {
        return Err("manifest must include sibling devmanager and devmanager-host".into());
    }
    if names
        .iter()
        .any(|name| name.contains("next") || name.contains("legacy"))
    {
        return Err("manifest must not include next/legacy development binaries".into());
    }

    if !manifest["webview2"]["windowsExpectation"]
        .as_str()
        .unwrap_or_default()
        .contains("WebView2")
    {
        return Err("manifest must declare WebView2 expectation".into());
    }

    Ok(())
}

#[test]
fn package_contract_source_declares_sibling_client_and_host_only() {
    let contract = package_contract();
    let cargo = cargo_toml();
    let version = cargo_package_version(&cargo);

    assert_eq!(contract["productName"], "DevManager");
    assert_eq!(contract["identifier"], "com.userfirst.devmanager");
    assert_eq!(contract["binariesDir"], "target/release");
    assert_eq!(contract["protocol"]["major"], 1);
    assert_eq!(contract["protocol"]["minor"], 0);
    assert_eq!(
        contract["shippingIdentity"]["format"],
        "devmanager/{version}"
    );
    assert_eq!(
        contract["shippingIdentity"]["clientBuild"],
        "devmanager/{version}"
    );
    assert_eq!(contract["publication"]["autoPublishPublic"], false);
    assert_eq!(contract["publication"]["requiresExplicitTagInput"], true);
    assert!(
        !contract["shippingIdentity"]["clientBuild"]
            .as_str()
            .unwrap()
            .contains("next"),
        "package tests must not preserve devmanager-next shipping identity"
    );

    let before = contract["beforePackagingCommand"]
        .as_str()
        .expect("beforePackagingCommand");
    assert!(
        cargo.contains(&format!("before-packaging-command = \"{before}\"")),
        "Cargo.toml before-packaging-command must build both shipping binaries once"
    );
    assert!(cargo.contains("binaries-dir = \"target/release\""));
    assert!(before.contains("--bin devmanager"));
    assert!(before.contains("--bin devmanager-host"));
    assert!(!before.contains("devmanager-next"));

    assert_packager_binary(&cargo, "devmanager", true);
    assert_packager_binary(&cargo, "devmanager-host", false);
    assert!(
        !cargo.contains("path = \"devmanager-next\"\nmain"),
        "packager binary list must omit devmanager-next"
    );

    for icon in contract["icons"].as_array().expect("icons") {
        let path = repo_root().join(icon.as_str().expect("icon path"));
        assert!(path.is_file(), "missing icon {}", path.display());
    }
    for resource in contract["resources"].as_array().expect("resources") {
        let path = repo_root().join(resource.as_str().expect("resource path"));
        assert!(path.exists(), "missing resource {}", path.display());
    }

    let exclusions = read(&repo_root().join("packaging/exclusions.txt"));
    for required in [
        ".worktrees",
        "target",
        "evidence",
        "tests/fixtures",
        "session.json",
        "dev-profile",
        "devmanager-next",
        "Portal",
        "secrets",
        "zz-archive",
    ] {
        assert!(
            exclusions.lines().any(|line| line.trim() == required),
            "exclusions.txt must list {required}"
        );
    }
    for required in contract["excludes"].as_array().expect("excludes") {
        let token = required.as_str().expect("exclude token");
        assert!(
            exclusions.lines().any(|line| line.trim() == token),
            "exclusions.txt must list contract exclude {token}"
        );
    }

    let capabilities = read(&repo_root().join("src/protocol/capabilities.rs"));
    assert!(capabilities.contains("pub const PROTOCOL_MAJOR: u16 = 1;"));
    assert!(capabilities.contains("pub const PROTOCOL_MINOR: u16 = 0;"));
    assert_eq!(
        version,
        cargo_package_version(&cargo),
        "semantic version identity must stay single-sourced from Cargo.toml"
    );

    assert!(
        contract["webview2"]["windowsExpectation"]
            .as_str()
            .unwrap_or_default()
            .contains("WebView2"),
        "contract must declare WebView2 expectation"
    );
    assert_eq!(
        contract["hostCtlSmoke"],
        serde_json::json!(["ctl", "actions", "--json"])
    );
}

#[test]
fn package_contract_windows_metadata_is_stamped_per_shipping_binary() {
    let build_rs = read(&repo_root().join("build.rs"));
    let contract = package_contract();

    assert!(
        build_rs.contains("stamp_windows_binary(")
            && build_rs.contains("\"devmanager\"")
            && build_rs.contains("\"devmanager.exe\""),
        "build.rs must stamp Windows metadata for devmanager only"
    );
    assert!(
        build_rs.contains("stamp_windows_binary(")
            && build_rs.contains("\"devmanager-host\"")
            && build_rs.contains("\"devmanager-host.exe\""),
        "build.rs must stamp Windows metadata for devmanager-host only"
    );
    assert!(build_rs.contains("DevManager Host"));
    assert!(build_rs.contains("devmanager-host.exe"));
    assert!(build_rs.contains("OriginalFilename"));
    assert!(build_rs.contains("DEVMANAGER_SHIPPING_IDENTITY"));

    let client = &contract["binaries"][0]["windows"];
    let host = &contract["binaries"][1]["windows"];
    assert_eq!(client["fileDescription"], "DevManager");
    assert_eq!(host["fileDescription"], "DevManager Host");
    assert_eq!(client["productName"], host["productName"]);
    assert_ne!(client["originalFilename"], host["originalFilename"]);
}

#[test]
fn package_manifest_fixture_accepts_sibling_identity_and_rejects_legacy_or_missing_host() {
    let contract = package_contract();
    let cargo_toml = cargo_toml();
    let version = cargo_package_version(&cargo_toml);
    let fixtures = repo_root().join("tests/fixtures/package");

    validate_manifest(
        &read_json(&fixtures.join("valid-windows-x86_64.manifest.json")),
        &contract,
        version,
    )
    .expect("valid manifest fixture must pass");

    let legacy = validate_manifest(
        &read_json(&fixtures.join("legacy-binary-present.manifest.json")),
        &contract,
        version,
    );
    assert!(
        legacy.is_err(),
        "legacy binary fixture must fail closed: {legacy:?}"
    );

    let missing_host = validate_manifest(
        &read_json(&fixtures.join("host-missing.manifest.json")),
        &contract,
        version,
    );
    assert!(
        missing_host.is_err(),
        "host-missing fixture must fail closed: {missing_host:?}"
    );
}

#[test]
fn package_docs_and_workflows_describe_one_product_two_binaries_without_next_identity() {
    let readme = read(&repo_root().join("README.md"));
    assert!(readme.contains("devmanager.exe"));
    assert!(readme.contains("devmanager-host.exe"));
    assert!(readme.contains("devmanager/"));
    assert!(
        !readme.contains("devmanager-next.exe"),
        "README must not advertise the development-only next binary as the product"
    );
    assert!(
        readme.contains("manual approval") || readme.contains("release-publish"),
        "README must describe protected/manual publication"
    );

    let architecture = read(&repo_root().join("docs/architecture.md"));
    assert!(architecture.contains("devmanager-host"));
    assert!(architecture.contains("devmanager/"));
    assert!(architecture.contains("GPUI"));

    let connect = read(&repo_root().join("docs/connect.md"));
    assert!(connect.contains("pairing"));
    assert!(connect.to_lowercase().contains("invitation"));

    let checklist = read(&repo_root().join("docs/release-checklist.md"));
    assert!(checklist.contains("latest.json"));
    assert!(checklist.contains("devmanager-host"));
    assert!(checklist.contains("sha256") || checklist.contains("immutable"));
    assert!(checklist.contains("release-publish") || checklist.contains("manual"));

    let release = read(&repo_root().join(".github/workflows/release.yml"));
    assert!(release.contains("Assert-PackageContract.ps1"));
    assert!(release.contains("latest.json"));
    assert!(release.contains("verify-packager-signatures"));
    assert!(release.contains("cargo metadata --format-version 1 --locked"));
    assert!(release.contains("environment: release-publish"));
    assert!(
        release.contains("CARGO_PACKAGER_SIGN_PRIVATE_KEY"),
        "release workflow must preserve signed updater artifact generation"
    );
    assert!(
        release.contains("publish-assets-expected-names.txt"),
        "draft verification must derive the exact emitted publish-assets name set"
    );
    assert!(
        !release.contains("Expected exactly 11 non-empty staged release assets"),
        "draft verification must not keep the stale 11-asset hard-coded expectation"
    );
    assert!(
        release.contains("14 uniquely named platform")
            && release.contains("plus latest.json"),
        "staging must document the 14 platform artifacts + latest.json emit contract"
    );

    let notices = read(&repo_root().join("THIRD_PARTY_NOTICES.md"));
    assert!(
        notices.to_lowercase().contains("gpui") || notices.contains("gpui-component"),
        "third-party notices must cover the GPUI UI stack in use"
    );
    assert!(
        notices.to_lowercase().contains("sqlite") || notices.contains("rusqlite"),
        "third-party notices must cover SQLite via rusqlite"
    );
    assert!(
        notices.contains("similar")
            && (notices.contains("NOT_LOCKED") || notices.contains("3.1.2")),
        "third-party notices must cover similar 3.1.2 conditionally on lock membership"
    );
    assert!(
        notices.contains("0.23.37") && notices.contains("rustls"),
        "third-party notices must cover exact locked rustls"
    );
    assert!(
        release.contains("recompute") || release.contains("recomputed"),
        "publish must recompute artifact hashes before publication"
    );
    assert!(
        release.contains("inputs.tag_name") || release.contains("steps.selected.outputs.tag_name"),
        "publish must require an explicit selected draft tag"
    );
    assert!(
        !release
            .split("\n  publish:")
            .nth(1)
            .expect("publish job")
            .split("\n    steps:")
            .next()
            .expect("publish header")
            .contains("needs:"),
        "publish job must remain independent from stage/prepare"
    );
}

#[test]
fn stale_reference_scan_keeps_forbidden_patterns_and_exact_path_token_safety_contracts() {
    let contract = package_contract();
    let scan = &contract["staleReferenceScan"];
    let patterns = scan["forbiddenPatterns"]
        .as_array()
        .expect("forbiddenPatterns");
    for required in [
        "devmanager-next",
        "session.json",
        "#[tauri::command]",
        "tauri::command",
        "src/sessions/",
        "web/src/sessions/",
    ] {
        assert!(
            patterns.iter().any(|value| value == required),
            "forbiddenPatterns must retain {required}"
        );
    }

    let intentional = scan["intentionalSafetyReferences"]
        .as_array()
        .expect("intentionalSafetyReferences");
    let expected = [
        ("src/updater/handoff.rs", &["session.json"][..]),
        ("tests/prompt_ui.rs", &["session.json"][..]),
        ("tests/update_contract.rs", &["session.json"][..]),
        (
            "scripts/native-next/Invoke-CutoverAudit.ps1",
            &["session.json", "devmanager-next"][..],
        ),
    ];
    assert_eq!(intentional.len(), expected.len());
    for (path, tokens) in expected {
        let entry = intentional
            .iter()
            .find(|value| value["path"] == path)
            .unwrap_or_else(|| panic!("missing intentional safety path {path}"));
        let declared = entry["tokens"].as_array().expect("tokens");
        assert_eq!(declared.len(), tokens.len(), "token count for {path}");
        for token in tokens {
            assert!(
                declared.iter().any(|value| value == *token),
                "{path} must intentionally allow exact token {token}"
            );
            assert!(
                patterns.iter().any(|value| value == *token),
                "intentional token {token} must remain a forbidden pattern"
            );
        }
        assert!(
            repo_root().join(path).is_file(),
            "intentional safety path must exist: {path}"
        );
    }

    let scanner = read(&repo_root().join("packaging/Assert-StaleReferences.ps1"));
    assert!(
        scanner.contains("Test-IntentionalSafetyReference")
            && scanner.contains("intentionalSafetyReferences"),
        "stale scanner must enforce exact path+token intentional contracts"
    );
    assert!(
        scanner.contains("must not blanket-allow undeclared tokens"),
        "stale scanner must self-check that intentional contracts stay path+token narrow"
    );
}
