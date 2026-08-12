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
    update_inspection_from_host_quit, HostConnectionUpdateProbe, HostQuitInspectionSource,
    HostUpdateAdmission, HostUpdateHandoff,
};
use devmanager::updater::{
    apply_cache_busting_to_packager_config, assert_atomic_installer_bundle,
    classify_user_state_path, evaluate_release_candidate, extract_build_version,
    is_remote_version_newer, package_identity_for_version, parse_release_manifest, parse_semver,
    prefer_signed_manifest_over_stale_cache, resolve_running_package_identity, update_state_policy,
    validate_preservation_checkpoint, ActiveUpdateResource, AtomicInstallerBundle,
    CacheBustingRequestPolicy, FixedActiveResourceProbe, HandoffBlockReason,
    IdentityPreservationReport, IgnoredUserStateKind, InstallUpdateOptions, PackageVersionSource,
    PreservationCheckpoint, PreservedUserStateKind, SilentReplacementDecision, UpdateCutoverKind,
    UpdateHandoffError, UpdateHandoffMachine, UpdateHandoffPhase, UpdateRejection,
    UpdateResourceInspection, UpdaterService, UserStateClassification,
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

    let cases: CaseFile =
        serde_json::from_str(&read_fixture("prerelease-ordering.json")).expect("cases");
    for case in cases.cases {
        let observed = is_remote_version_newer(&case.current, &case.remote).expect("compare");
        assert_eq!(
            observed, case.newer,
            "ordering mismatch for {} vs {}",
            case.current, case.remote
        );
        let current = parse_semver(&case.current).expect("parse current");
        let remote = parse_semver(&case.remote).expect("parse remote");
        assert_eq!(remote > current, case.newer);
    }
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
        let found = policy
            .header_pairs()
            .iter()
            .find(|(name, _)| *name == key.as_str());
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
fn host_client_build_mismatch_is_rejected() {
    let current = package_identity_for_version("0.4.1", PackageVersionSource::BinaryMetadata)
        .expect("identity");
    let manifest =
        parse_release_manifest(&read_fixture("host-client-mismatch.json")).expect("manifest");
    let rejected = evaluate_release_candidate(&current, &manifest, "windows-x86_64")
        .expect_err("host/client mismatch");
    assert!(matches!(
        rejected,
        UpdateRejection::HostClientMismatch {
            ref client_build,
            ref host_build
        } if client_build == "devmanager/0.4.2" && host_build == "devmanager-host/0.4.1"
    ));
}

#[test]
fn atomic_installer_bundle_requires_matching_pair_and_packager_verification() {
    let ok = AtomicInstallerBundle::for_verified_packager_update("0.4.2", 1, 0, None)
        .expect("verified bundle");
    assert_atomic_installer_bundle(&ok).expect("assert");

    let mut unverified = ok.clone();
    unverified.signature_verified_by_packager = false;
    assert!(assert_atomic_installer_bundle(&unverified).is_err());

    let mut mismatch = ok;
    mismatch.host_build = "devmanager-host/0.4.1".into();
    assert!(assert_atomic_installer_bundle(&mismatch).is_err());
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
fn install_update_requires_handoff_probe_and_refuses_bypass_without_it() {
    let updater = UpdaterService::new();
    let err = updater
        .install_update_with_options(InstallUpdateOptions::default())
        .expect_err("install without ready update / probe");
    assert!(
        err.contains("ready to install") || err.contains("Active resource probe"),
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
fn host_quit_source_probe_drains_confirms_and_aborts_before_irreversible() {
    use devmanager::domain::host::{HostQuitInspection, HostQuitWorktreeInspection};

    struct FixedQuit(HostQuitInspection);
    impl HostQuitInspectionSource for FixedQuit {
        fn inspect_host_quit_for_update(&mut self) -> Result<HostQuitInspection, String> {
            Ok(self.0.clone())
        }
    }

    let boot = Uuid::now_v7();
    let mut source = FixedQuit(HostQuitInspection {
        inspection_id: 42,
        agents: Vec::new(),
        resources: Vec::new(),
        worktrees: HostQuitWorktreeInspection::NotInspected,
        confirmable: true,
    });
    let mapped = update_inspection_from_host_quit(&source.0, boot);
    assert!(mapped.active.is_empty());

    let mut probe = HostConnectionUpdateProbe::new(&mut source, boot);
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
fn explicit_confirm_reaches_irreversible_then_resync() {
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
        .expect("irreversible");
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
fn preservation_checkpoint_old_to_new_and_new_to_new() {
    let expectation: serde_json::Value =
        serde_json::from_str(&read_fixture("old-to-new/expectation.json")).expect("expectation");
    let config = read_fixture("old-to-new/config.json");
    let remote = read_fixture("old-to-new/remote.json");
    let session = read_fixture("old-to-new/session.json");

    assert_eq!(
        classify_user_state_path("session.json"),
        Some(UserStateClassification::Ignore(
            IgnoredUserStateKind::SessionJson
        ))
    );

    let old_to_new = PreservationCheckpoint {
        cutover: UpdateCutoverKind::OldToNew,
        report: IdentityPreservationReport {
            config_hash_before: sha256_hex(config.as_bytes()),
            config_hash_after: sha256_hex(config.as_bytes()),
            remote_hash_before: sha256_hex(remote.as_bytes()),
            remote_hash_after: sha256_hex(remote.as_bytes()),
            device_pairing_fingerprint_before: expectation["hashes"]["devicePairingFingerprint"]
                .as_str()
                .unwrap()
                .to_string(),
            device_pairing_fingerprint_after: expectation["hashes"]["devicePairingFingerprint"]
                .as_str()
                .unwrap()
                .to_string(),
            task_db_hash_before: None,
            task_db_hash_after: None,
            session_json_considered: false,
            legacy_conversations_imported: false,
        },
        old_binaries_usable_on_failure: true,
    };
    validate_preservation_checkpoint(&old_to_new).expect("old-to-new");
    assert!(session.contains("legacy-session-a"));

    let db = read_fixture("new-to-new/task-prompt-db.json");
    let db_hash = sha256_hex(db.as_bytes());
    let new_to_new = PreservationCheckpoint {
        cutover: UpdateCutoverKind::NewToNew,
        report: IdentityPreservationReport {
            config_hash_before: sha256_hex(read_fixture("new-to-new/config.json").as_bytes()),
            config_hash_after: sha256_hex(read_fixture("new-to-new/config.json").as_bytes()),
            remote_hash_before: sha256_hex(read_fixture("new-to-new/remote.json").as_bytes()),
            remote_hash_after: sha256_hex(read_fixture("new-to-new/remote.json").as_bytes()),
            device_pairing_fingerprint_before:
                "pairing:host-stable-001:PAIR-KEEP-ME:device-phone-001".into(),
            device_pairing_fingerprint_after:
                "pairing:host-stable-001:PAIR-KEEP-ME:device-phone-001".into(),
            task_db_hash_before: Some(db_hash.clone()),
            task_db_hash_after: Some(db_hash),
            session_json_considered: false,
            legacy_conversations_imported: false,
        },
        old_binaries_usable_on_failure: true,
    };
    validate_preservation_checkpoint(&new_to_new).expect("new-to-new");

    let (preserve, ignore) = update_state_policy();
    assert!(preserve.contains(&PreservedUserStateKind::TaskPromptDatabase));
    assert!(ignore.contains(&IgnoredUserStateKind::SessionJson));
}
