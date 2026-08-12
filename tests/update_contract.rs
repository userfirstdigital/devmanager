//! Phase 11.6 update detection and bounded handoff contracts.
//!
//! Source/fixture contracts only — no network or installer execution.
//! Cryptographic signature success is modeled as packager-verified download
//! results, not manifest string presence.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cargo_packager_updater::{
    url::Url, Config as PackagerUpdaterConfig, WindowsConfig as PackagerWindowsConfig,
};
use devmanager::host::{
    owned_probe_from_quit_inspection, update_inspection_from_host_quit, HostUpdateAdmission,
    HostUpdateHandoff, HostUpdateRuntimeGate, OwnedActiveResourceProbe,
};
use devmanager::updater::{
    apply_cache_busting_to_packager_config, assert_atomic_installer_bundle,
    capture_preservation_checkpoint, classify_user_state_path,
    clear_update_handoff_recovery_marker, evaluate_release_candidate, extract_build_version,
    inspect_atomic_installer_payload_dir, is_remote_version_newer, package_identity_for_version,
    packager_architecture_target, packager_os_target, parse_release_manifest,
    persist_update_handoff_recovery_marker, prefer_signed_manifest_over_stale_cache,
    read_update_handoff_recovery_marker, resolve_running_package_identity, update_state_policy,
    validate_preservation_checkpoint, verify_downloaded_artifact_sha256, ActiveUpdateResource,
    AtomicInstallerBundle, CacheBustingRequestPolicy, FixedActiveResourceProbe, HandoffBlockReason,
    IgnoredUserStateKind, InstallUpdateOptions, PackageVersionSource, PreservedUserStateKind,
    SilentReplacementDecision, StagedBinaryReplacement, StagedReplacePhase, UpdateCutoverKind,
    UpdateHandoffError, UpdateHandoffMachine, UpdateHandoffPhase, UpdateHandoffRecoveryMarker,
    UpdateHandoffToken, UpdateRejection, UpdateResourceInspection, UpdaterService,
    UserStateClassification,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("update")
}

fn read_fixture(relative: &str) -> String {
    fs::read_to_string(fixture_root().join(relative)).unwrap_or_else(|error| {
        panic!("failed to read update fixture {relative}: {error}");
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Contract fixture identity — public fields only; not a cryptographic proof path.
fn contract_bundle(version: &str, hash: String) -> AtomicInstallerBundle {
    let bundle = AtomicInstallerBundle {
        version: version.into(),
        client_exe: "devmanager.exe".into(),
        host_exe: "devmanager-host.exe".into(),
        client_build: format!("devmanager/{version}"),
        host_build: format!("devmanager-host/{version}"),
        protocol_major: 1,
        protocol_minor: 0,
        artifact_hash: Some(hash),
        signature_verified_by_packager: true,
        packager_target: "windows-x86_64".into(),
        download_url: format!("https://example.com/devmanager-{version}.zip"),
        signature: "dGVzdC1zaWduYXR1cmUtcGF5bG9hZC1mb3ItY29udHJhY3Q=".into(),
        format: "zip".into(),
    };
    assert_atomic_installer_bundle(&bundle).expect("contract bundle shape");
    bundle
}

#[test]
fn installed_0_4_1_admits_newer_matching_manifest_fields() {
    let current = package_identity_for_version("0.4.1", PackageVersionSource::BinaryMetadata)
        .expect("0.4.1 identity");
    let manifest = parse_release_manifest(&read_fixture("latest-0.4.2.json")).expect("manifest");
    let admitted =
        evaluate_release_candidate(&current, &manifest, "windows-x86_64").expect("admit 0.4.2");
    assert_eq!(admitted.version.to_string(), "0.4.2");
    assert_eq!(admitted.client_build, "devmanager/0.4.2");
    assert_eq!(admitted.host_build, "devmanager-host/0.4.2");
    assert_eq!(admitted.minimum_protocol.as_deref(), Some("1.0"));
    assert_eq!(admitted.format.as_deref(), Some("nsis"));
    assert!(admitted.hash.as_deref().unwrap().starts_with("sha256:"));
    // AdmittedUpdate carries the signature *field* for packager download; it is
    // not a claim of cryptographic verification.
    assert!(!admitted.signature.is_empty());
}

#[test]
fn prerelease_and_build_metadata_ordering_matches_one_semver_impl() {
    #[derive(Deserialize)]
    struct CaseFile {
        cases: Vec<Case>,
    }
    #[derive(Deserialize)]
    struct Case {
        current: String,
        remote: String,
        newer: bool,
    }
    let file: CaseFile =
        serde_json::from_str(&read_fixture("prerelease-ordering.json")).expect("cases");
    for case in file.cases {
        assert_eq!(
            is_remote_version_newer(&case.current, &case.remote).expect("compare versions"),
            case.newer,
            "{} vs {}",
            case.current,
            case.remote
        );
    }
}

#[test]
fn packager_architecture_target_key_is_os_arch() {
    let Some(target) = packager_architecture_target() else {
        return;
    };
    assert!(
        target.contains('-'),
        "packager target must be OS-ARCH, got {target}"
    );
}

#[test]
fn packager_update_target_is_os_only_while_manifest_key_is_os_arch() {
    let Some(os) = packager_os_target() else {
        return;
    };
    let Some(arch) = packager_architecture_target() else {
        return;
    };
    assert_eq!(os, arch.split('-').next().unwrap_or_default());
    assert_ne!(
        os, arch,
        "request target and manifest key must stay distinct"
    );
}

#[test]
fn recovery_marker_round_trip_validates_hello_and_clears() {
    let root = std::env::temp_dir().join(format!(
        "devmanager-recovery-marker-{}",
        Uuid::now_v7().as_u128()
    ));
    fs::create_dir_all(&root).unwrap();
    let token = UpdateHandoffToken {
        token_id: Uuid::now_v7(),
        host_boot_id: Uuid::now_v7(),
        inspection_id: 7,
        target_version: "0.4.2".into(),
        client_build: "devmanager/0.4.2".into(),
        host_build: "devmanager-host/0.4.2".into(),
        issued_at: UNIX_EPOCH + Duration::from_secs(1),
        expires_at: UNIX_EPOCH + Duration::from_secs(120),
    };
    let marker = UpdateHandoffRecoveryMarker::from_token(&token, 1, 0);
    assert_eq!(marker.token_id, token.token_id);
    assert_eq!(marker.host_boot_id, token.host_boot_id);
    assert_eq!(marker.inspection_id, token.inspection_id);
    persist_update_handoff_recovery_marker(&root, &marker).expect("persist");
    let loaded = read_update_handoff_recovery_marker(&root)
        .expect("read")
        .expect("present");
    loaded
        .validate_live_host_hello("devmanager-host/0.4.2", 1, 0)
        .expect("hello match");
    assert!(loaded
        .validate_live_host_hello("devmanager-host/0.4.1", 1, 0)
        .is_err());

    let gate = HostUpdateRuntimeGate::new();
    let now = UNIX_EPOCH + Duration::from_secs(10);
    gate.complete_recovery_from_marker(&loaded, "devmanager-host/0.4.2", 1, 0, now, || {
        clear_update_handoff_recovery_marker(&root)
    })
    .expect("complete recovery");
    assert!(read_update_handoff_recovery_marker(&root)
        .expect("reread")
        .is_none());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn recovery_marker_rejects_lineage_or_build_mismatch() {
    let token = UpdateHandoffToken {
        token_id: Uuid::now_v7(),
        host_boot_id: Uuid::now_v7(),
        inspection_id: 19,
        target_version: "0.4.2".into(),
        client_build: "devmanager/0.4.2".into(),
        host_build: "devmanager-host/0.4.2".into(),
        issued_at: UNIX_EPOCH + Duration::from_secs(1),
        expires_at: UNIX_EPOCH + Duration::from_secs(120),
    };
    let mut marker = UpdateHandoffRecoveryMarker::from_token(&token, 1, 0);
    marker.host_build = "devmanager-host/0.4.1".into();
    assert!(marker
        .validate_live_host_hello("devmanager-host/0.4.1", 1, 0)
        .is_err());

    let mut marker = UpdateHandoffRecoveryMarker::from_token(&token, 1, 0);
    marker.protocol_minor = 1;
    assert!(marker
        .validate_live_host_hello("devmanager-host/0.4.2", 1, 0)
        .is_err());
}

#[test]
fn updater_check_path_applies_cache_busting_to_packager_endpoints() {
    #[derive(Deserialize)]
    struct PolicyFixture {
        endpoint: String,
        required_request_headers: std::collections::HashMap<String, String>,
        cache_bust_query_param: String,
    }

    let fixture: PolicyFixture =
        serde_json::from_str(&read_fixture("endpoint-cache-policy.json")).expect("policy");
    let now = UNIX_EPOCH + Duration::from_millis(1_723_456_789_012);
    let policy = CacheBustingRequestPolicy::for_instant(now);
    assert_eq!(policy.query_param, fixture.cache_bust_query_param);

    let config = PackagerUpdaterConfig {
        endpoints: vec![Url::parse(&fixture.endpoint).expect("endpoint")],
        pubkey: "test-pubkey".into(),
        windows: Some(PackagerWindowsConfig {
            installer_args: None,
            install_mode: None,
        }),
    };
    let busted = apply_cache_busting_to_packager_config(config, &policy).expect("apply");
    assert_eq!(busted.endpoints.len(), 1);
    let url = busted.endpoints[0].as_str();
    assert!(
        url.contains(&format!("{}={}", policy.query_param, policy.nonce)),
        "UpdaterService check path must mutate endpoint URLs: {url}"
    );
    for (key, expected) in &fixture.required_request_headers {
        let header_pairs = policy.header_pairs();
        let found = header_pairs.iter().find(|(name, _)| *name == key.as_str());
        assert_eq!(found.map(|(_, value)| *value), Some(expected.as_str()));
    }
}

#[test]
fn stale_cached_metadata_loses_to_fresher_signed_body() {
    let stale = parse_release_manifest(&read_fixture("stale-cached-0.4.1.json")).expect("stale");
    let fresh = parse_release_manifest(&read_fixture("latest-0.4.2.json")).expect("fresh");
    let selected = prefer_signed_manifest_over_stale_cache(&stale, &fresh).expect("prefer fresh");
    assert_eq!(selected.version, "0.4.2");
}

#[test]
fn malformed_manifest_signature_field_is_prefiltered() {
    let current = package_identity_for_version("0.4.1", PackageVersionSource::BinaryMetadata)
        .expect("identity");
    let manifest =
        parse_release_manifest(&read_fixture("corrupt-signature.json")).expect("manifest");
    let rejected = evaluate_release_candidate(&current, &manifest, "windows-x86_64")
        .expect_err("malformed signature field");
    assert!(matches!(
        rejected,
        UpdateRejection::MalformedManifestField { .. }
    ));
}

#[test]
fn downgrade_is_rejected() {
    let current = package_identity_for_version("0.4.1", PackageVersionSource::BinaryMetadata)
        .expect("identity");
    let manifest = parse_release_manifest(&read_fixture("downgrade-0.4.0.json")).expect("manifest");
    let rejected =
        evaluate_release_candidate(&current, &manifest, "windows-x86_64").expect_err("downgrade");
    assert!(matches!(
        rejected,
        UpdateRejection::Downgrade {
            current: ref c,
            remote: ref r
        } if c == "0.4.1" && r == "0.4.0"
    ));
}

#[test]
fn matching_version_is_rejected() {
    let current = package_identity_for_version("0.4.1", PackageVersionSource::BinaryMetadata)
        .expect("identity");
    let manifest =
        parse_release_manifest(&read_fixture("same-version-0.4.1.json")).expect("manifest");
    let rejected = evaluate_release_candidate(&current, &manifest, "windows-x86_64")
        .expect_err("matching version");
    assert!(matches!(
        rejected,
        UpdateRejection::MatchingVersion { ref version } if version == "0.4.1"
    ));
}

#[test]
fn evaluate_rejects_host_client_mismatch_manifest() {
    let current = package_identity_for_version("0.4.1", PackageVersionSource::BinaryMetadata)
        .expect("identity");
    let manifest =
        parse_release_manifest(&read_fixture("host-client-mismatch.json")).expect("manifest");
    assert!(matches!(
        evaluate_release_candidate(&current, &manifest, "windows-x86_64"),
        Err(UpdateRejection::HostClientMismatch {
            ref client_build,
            ref host_build
        }) if client_build == "devmanager/0.4.2" && host_build == "devmanager-host/0.4.1"
    ));
}

#[test]
fn evaluate_requires_sha256_protocol_and_build_identity() {
    let current = package_identity_for_version("0.4.1", PackageVersionSource::BinaryMetadata)
        .expect("identity");
    let mut manifest =
        parse_release_manifest(&read_fixture("latest-0.4.2.json")).expect("manifest");
    manifest.platforms.get_mut("windows-x86_64").unwrap().hash = None;
    assert!(matches!(
        evaluate_release_candidate(&current, &manifest, "windows-x86_64"),
        Err(UpdateRejection::MissingRequiredSha256)
    ));
}

#[test]
fn downloaded_bytes_must_match_required_sha256() {
    let expected = sha256_hex(b"installer-bytes");
    assert_eq!(
        verify_downloaded_artifact_sha256(b"installer-bytes", &expected).unwrap(),
        expected
    );
    assert!(verify_downloaded_artifact_sha256(b"tampered", &expected).is_err());
}

#[test]
fn atomic_installer_bundle_requires_matching_pair_and_packager_verification() {
    let hash = format!("sha256:{}", "b".repeat(64));
    let ok = contract_bundle("0.4.2", hash);
    assert_atomic_installer_bundle(&ok).expect("assert");

    let mut unverified = ok.clone();
    unverified.signature_verified_by_packager = false;
    assert!(assert_atomic_installer_bundle(&unverified).is_err());

    let mut mismatch = ok;
    mismatch.host_build = "devmanager-host/0.4.1".into();
    assert!(assert_atomic_installer_bundle(&mismatch).is_err());
}

#[test]
fn staged_payload_dir_must_contain_both_binaries() {
    let root = std::env::temp_dir().join(format!(
        "devmanager-bundle-inspect-{}",
        Uuid::now_v7().as_u128()
    ));
    let staged = root.join("staged");
    fs::create_dir_all(&staged).unwrap();
    fs::write(staged.join("devmanager.exe"), b"client").unwrap();
    let hash = sha256_hex(b"client");
    let bundle = contract_bundle("0.4.2", hash);
    assert!(inspect_atomic_installer_payload_dir(&staged, &bundle).is_err());
    fs::write(staged.join("devmanager-host.exe"), b"host").unwrap();
    inspect_atomic_installer_payload_dir(&staged, &bundle).expect("pair present");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn staged_two_binary_replace_rolls_back_on_missing_host() {
    let root = std::env::temp_dir().join(format!(
        "devmanager-staged-contract-{}",
        Uuid::now_v7().as_u128()
    ));
    let install = root.join("install");
    let staged = root.join("staged");
    fs::create_dir_all(&install).unwrap();
    fs::create_dir_all(&staged).unwrap();
    fs::write(install.join("devmanager.exe"), b"old-c").unwrap();
    fs::write(install.join("devmanager-host.exe"), b"old-h").unwrap();
    fs::write(staged.join("devmanager.exe"), b"new-c").unwrap();
    let hash = format!("sha256:{}", "c".repeat(64));
    let bundle = contract_bundle("0.4.2", hash);
    let replacement = StagedBinaryReplacement::new(&install, &staged, bundle);
    assert!(replacement.replace_with_rollback().is_err());
    assert_eq!(fs::read(install.join("devmanager.exe")).unwrap(), b"old-c");
    assert_eq!(
        fs::read(install.join("devmanager-host.exe")).unwrap(),
        b"old-h"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn durable_backups_precede_seal_shaped_commit() {
    let root = std::env::temp_dir().join(format!(
        "devmanager-durable-stage-{}",
        Uuid::now_v7().as_u128()
    ));
    let install = root.join("install");
    let staged = root.join("staged");
    fs::create_dir_all(&install).unwrap();
    fs::create_dir_all(&staged).unwrap();
    fs::write(install.join("devmanager.exe"), b"old-c").unwrap();
    fs::write(install.join("devmanager-host.exe"), b"old-h").unwrap();
    fs::write(staged.join("devmanager.exe"), b"new-c").unwrap();
    fs::write(staged.join("devmanager-host.exe"), b"new-h").unwrap();
    let bundle = contract_bundle("0.4.2", format!("sha256:{}", "d".repeat(64)));
    let replacement = StagedBinaryReplacement::new(&install, &staged, bundle);
    let progress = replacement.prepare_durable_backups().expect("backups");
    assert_eq!(progress.phase, StagedReplacePhase::BackedUp);
    assert!(replacement.stage_marker_path().exists());
    assert_eq!(fs::read(install.join("devmanager.exe")).unwrap(), b"old-c");
    replacement.commit_after_durable_backups().expect("commit");
    assert_eq!(fs::read(install.join("devmanager.exe")).unwrap(), b"new-c");
    assert_eq!(
        fs::read(install.join("devmanager-host.exe")).unwrap(),
        b"new-h"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn running_package_identity_derives_from_package_or_binary_metadata() {
    let identity = resolve_running_package_identity();
    assert!(matches!(
        identity.source,
        PackageVersionSource::BinaryMetadata | PackageVersionSource::EmbeddedPackageMetadata
    ));
    let version = identity.version.to_string();
    assert_eq!(
        extract_build_version(&identity.client_build),
        Some(version.as_str())
    );
    assert_eq!(
        extract_build_version(&identity.host_build),
        Some(version.as_str())
    );
}

#[test]
fn install_update_requires_owned_handoff_probe_and_refuses_bypass_without_it() {
    let updater = UpdaterService::new();
    let err = updater
        .install_update_with_options(InstallUpdateOptions::default())
        .expect_err("install without ready update / probe");
    assert!(
        err.contains("ready to install")
            || err.contains("Active resource probe")
            || err.contains("Host update gate"),
        "unexpected error: {err}"
    );
}

#[test]
fn active_resource_handoff_refuses_unsafe_silent_replacement() {
    let inspection = UpdateResourceInspection {
        inspection_id: 11,
        host_boot_id: Uuid::now_v7(),
        active: vec![ActiveUpdateResource {
            resource_id: "term-1".into(),
            kind: "terminal".into(),
            lifecycle: "Active".into(),
            task_id: Some("task-1".into()),
        }],
        confirmable: true,
    };
    assert!(matches!(
        UpdateHandoffMachine::decide_silent_replacement(
            &inspection,
            "devmanager/0.4.2",
            "devmanager-host/0.4.2",
        ),
        SilentReplacementDecision::Refused {
            block: HandoffBlockReason::ActiveResources { .. }
        }
    ));

    let mut handoff = HostUpdateHandoff::default();
    let mut probe = FixedActiveResourceProbe {
        inspection: inspection.clone(),
    };
    let err = handoff
        .run_pre_install_gate(
            &mut probe,
            "0.4.2",
            "devmanager/0.4.2",
            "devmanager-host/0.4.2",
            SystemTime::now(),
            false,
        )
        .expect_err("must refuse silent replacement");
    assert!(matches!(
        err,
        UpdateHandoffError::Blocked(HandoffBlockReason::UnsafeSilentReplacement)
    ));
}

#[test]
fn owned_host_quit_probe_drains_confirms_and_aborts_before_irreversible() {
    use devmanager::domain::host::{HostQuitInspection, HostQuitWorktreeInspection};

    let boot = Uuid::now_v7();
    let quit = HostQuitInspection {
        inspection_id: 42,
        agents: Vec::new(),
        resources: Vec::new(),
        worktrees: HostQuitWorktreeInspection::NotInspected,
        confirmable: true,
    };
    let mapped = update_inspection_from_host_quit(&quit, boot);
    assert!(mapped.active.is_empty());

    let mut probe = owned_probe_from_quit_inspection(quit, boot);
    let mut handoff = HostUpdateHandoff::new(Duration::from_secs(60));
    let now = UNIX_EPOCH + Duration::from_secs(100);
    let token = handoff
        .run_pre_install_gate(
            &mut probe,
            "0.4.2",
            "devmanager/0.4.2",
            "devmanager-host/0.4.2",
            now,
            false,
        )
        .expect("empty resources gate");
    assert!(!handoff.install_irreversible());
    assert_eq!(
        handoff.abort_pre_install().expect("abort"),
        HostUpdateAdmission::Ready
    );
    assert_eq!(token.host_boot_id, boot);
}

#[test]
fn runtime_gate_stops_new_launches_until_abort() {
    let gate = HostUpdateRuntimeGate::new();
    let boot = Uuid::now_v7();
    let mut probe = FixedActiveResourceProbe {
        inspection: UpdateResourceInspection {
            inspection_id: 1,
            host_boot_id: boot,
            active: Vec::new(),
            confirmable: true,
        },
    };
    assert!(!gate.stops_new_launches());
    let _ = gate
        .prepare_update(
            &mut probe,
            "0.4.2",
            "devmanager/0.4.2",
            "devmanager-host/0.4.2",
            UNIX_EPOCH + Duration::from_secs(5),
            false,
        )
        .expect("prepare");
    assert!(gate.stops_new_launches());
    assert_eq!(
        gate.abort_pre_install().expect("abort"),
        HostUpdateAdmission::Ready
    );
    assert!(!gate.stops_new_launches());
}

#[test]
fn explicit_confirm_reaches_irreversible_only_after_durable_seal() {
    let boot = Uuid::now_v7();
    let now = UNIX_EPOCH + Duration::from_secs(100);
    let mut handoff = HostUpdateHandoff::new(Duration::from_secs(60));
    let mut probe = FixedActiveResourceProbe {
        inspection: UpdateResourceInspection {
            inspection_id: 42,
            host_boot_id: boot,
            active: vec![ActiveUpdateResource {
                resource_id: "browser-1".into(),
                kind: "browser".into(),
                lifecycle: "Active".into(),
                task_id: Some("task-9".into()),
            }],
            confirmable: true,
        },
    };
    let token = handoff
        .run_pre_install_gate(
            &mut probe,
            "0.4.2",
            "devmanager/0.4.2",
            "devmanager-host/0.4.2",
            now,
            true,
        )
        .expect("explicit confirm path");
    handoff
        .begin_atomic_install(token.token_id, now + Duration::from_secs(2))
        .expect("arm install");
    assert!(!handoff.install_irreversible());
    assert!(handoff.abort_pre_install().is_ok());

    let mut handoff = HostUpdateHandoff::new(Duration::from_secs(60));
    let token = handoff
        .run_pre_install_gate(
            &mut probe,
            "0.4.2",
            "devmanager/0.4.2",
            "devmanager-host/0.4.2",
            now,
            true,
        )
        .expect("prepare again");
    handoff
        .begin_atomic_install(token.token_id, now + Duration::from_secs(2))
        .expect("arm");
    handoff.seal_after_durable_stage().expect("seal");
    assert!(handoff.install_irreversible());
    assert!(handoff.abort_pre_install().is_err());
    handoff
        .complete_matching_host_start(token.token_id, now + Duration::from_secs(3))
        .expect("reattach");
    handoff
        .finish_resync(token.token_id, now + Duration::from_secs(4))
        .expect("resync");
    assert_eq!(
        handoff.phase(),
        &UpdateHandoffPhase::Completed {
            installed_version: "0.4.2".into()
        }
    );
}

#[test]
fn preservation_checkpoint_reads_disposable_old_to_new_and_new_to_new_fixtures() {
    let old_root = fixture_root().join("old-to-new");
    let old = capture_preservation_checkpoint(&old_root, UpdateCutoverKind::OldToNew)
        .expect("old-to-new capture");
    validate_preservation_checkpoint(&old).expect("old-to-new validate");
    assert!(old.report.task_db_hash_before.is_none());
    assert_eq!(
        classify_user_state_path("session.json"),
        Some(UserStateClassification::Ignore(
            IgnoredUserStateKind::SessionJson
        ))
    );

    let new_root = fixture_root().join("new-to-new");
    let new = capture_preservation_checkpoint(&new_root, UpdateCutoverKind::NewToNew)
        .expect("new-to-new capture");
    validate_preservation_checkpoint(&new).expect("new-to-new validate");
    assert!(new.report.task_db_hash_before.is_some());

    let (preserve, ignore) = update_state_policy();
    assert!(preserve.contains(&PreservedUserStateKind::TaskPromptDatabase));
    assert!(ignore.contains(&IgnoredUserStateKind::SessionJson));
}

#[test]
fn owned_probe_is_send_static() {
    fn assert_send_static<T: Send + 'static>(_: T) {}
    let probe = OwnedActiveResourceProbe::from_fn(|| {
        Ok(UpdateResourceInspection {
            inspection_id: 1,
            host_boot_id: Uuid::nil(),
            active: Vec::new(),
            confirmable: true,
        })
    });
    assert_send_static(probe);
}

#[test]
fn interrupted_staged_replace_restores_old_host() {
    let root = std::env::temp_dir().join(format!(
        "devmanager-interrupted-{}",
        Uuid::now_v7().as_u128()
    ));
    let install = root.join("install");
    let staged = root.join("staged");
    fs::create_dir_all(&install).unwrap();
    fs::create_dir_all(&staged).unwrap();
    fs::write(install.join("devmanager.exe"), b"old-c").unwrap();
    fs::write(install.join("devmanager-host.exe"), b"old-h").unwrap();
    fs::write(staged.join("devmanager.exe"), b"new-c").unwrap();
    fs::write(staged.join("devmanager-host.exe"), b"new-h").unwrap();
    let bundle = contract_bundle("0.4.2", format!("sha256:{}", "e".repeat(64)));
    let replacement = StagedBinaryReplacement::new(&install, &staged, bundle);
    fs::copy(install.join("devmanager.exe"), replacement.client_backup()).unwrap();
    fs::copy(
        install.join("devmanager-host.exe"),
        replacement.host_backup(),
    )
    .unwrap();
    fs::write(install.join("devmanager.exe"), b"partial-client").unwrap();
    fs::write(replacement.stage_marker_path(), "client_replaced").unwrap();
    assert_eq!(
        replacement.recover_interrupted().unwrap(),
        StagedReplacePhase::RolledBack
    );
    assert_eq!(fs::read(install.join("devmanager.exe")).unwrap(), b"old-c");
    assert_eq!(
        fs::read(install.join("devmanager-host.exe")).unwrap(),
        b"old-h"
    );
    let _ = fs::remove_dir_all(root);
}
