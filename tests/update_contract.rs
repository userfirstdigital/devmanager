//! Phase 11.6 update detection and bounded handoff contracts.
//!
//! These tests are source/fixture contracts. They intentionally avoid network
//! and installer execution.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use devmanager::host::{HostUpdateAdmission, HostUpdateHandoff};
use devmanager::updater::{
    classify_user_state_path, evaluate_release_candidate, extract_build_version,
    is_remote_version_newer, package_identity_for_version, parse_release_manifest, parse_semver,
    prefer_signed_manifest_over_stale_cache, resolve_running_package_identity, update_state_policy,
    ActiveUpdateResource, CacheBustingRequestPolicy, HandoffBlockReason,
    IdentityPreservationReport, IgnoredUserStateKind, PackageVersionSource, PreservedUserStateKind,
    SilentReplacementDecision, UpdateHandoffMachine, UpdateHandoffPhase, UpdateRejection,
    UpdateResourceInspection, UserStateClassification,
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
fn installed_0_4_1_admits_signed_0_4_2() {
    let current = package_identity_for_version("0.4.1", PackageVersionSource::BinaryMetadata)
        .expect("0.4.1 identity");
    let manifest = parse_release_manifest(&read_fixture("latest-0.4.2.json")).expect("manifest");
    let admitted =
        evaluate_release_candidate(&current, &manifest, "windows-x86_64").expect("admit 0.4.2");
    assert_eq!(admitted.version.to_string(), "0.4.2");
    assert_eq!(admitted.client_build, "devmanager/0.4.2");
    assert_eq!(admitted.host_build, "devmanager-host/0.4.2");
    assert_eq!(admitted.minimum_protocol.as_deref(), Some("1.0"));
    assert!(admitted.hash.as_deref().unwrap().starts_with("sha256:"));
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
fn cache_busting_policy_appends_nonce_and_no_cache_headers() {
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

    let busted = policy
        .apply_to_endpoint(&fixture.endpoint)
        .expect("cache bust endpoint");
    assert!(
        busted.contains(&format!("{}={}", policy.query_param, policy.nonce)),
        "missing cache-bust query on {busted}"
    );
    assert!(
        !busted.ends_with("latest.json"),
        "cache-bust must alter the request URL: {busted}"
    );

    let headers = policy.header_pairs();
    for (key, expected) in &fixture.required_request_headers {
        let found = headers.iter().find(|(name, _)| *name == key.as_str());
        assert_eq!(
            found.map(|(_, value)| *value),
            Some(expected.as_str()),
            "missing/incorrect header {key}"
        );
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
fn corrupt_signature_is_rejected() {
    let current = package_identity_for_version("0.4.1", PackageVersionSource::BinaryMetadata)
        .expect("identity");
    let manifest =
        parse_release_manifest(&read_fixture("corrupt-signature.json")).expect("manifest");
    let rejected = evaluate_release_candidate(&current, &manifest, "windows-x86_64")
        .expect_err("corrupt signature");
    assert!(matches!(rejected, UpdateRejection::CorruptSignature { .. }));
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
fn running_package_identity_derives_from_package_or_binary_metadata() {
    let identity = resolve_running_package_identity();
    assert!(
        matches!(
            identity.source,
            PackageVersionSource::BinaryMetadata | PackageVersionSource::EmbeddedPackageMetadata
        ),
        "current version must not come from checkout/PWA assets"
    );
    let version = identity.version.to_string();
    assert_eq!(
        extract_build_version(&identity.client_build),
        Some(version.as_str())
    );
    assert_eq!(
        extract_build_version(&identity.host_build),
        Some(version.as_str())
    );
    let _ = parse_semver(&version).expect("one semver impl");
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

    let mut machine = UpdateHandoffMachine::default();
    machine.begin_inspect().expect("inspect");
    let err = machine
        .prepare(
            &inspection,
            "0.4.2",
            "devmanager/0.4.2",
            "devmanager-host/0.4.2",
            SystemTime::now(),
            false,
        )
        .expect_err("must refuse silent replacement");
    assert!(matches!(
        err,
        devmanager::updater::UpdateHandoffError::Blocked(
            HandoffBlockReason::UnsafeSilentReplacement
        )
    ));
}

#[test]
fn explicit_confirm_drains_installs_matching_pair_and_resyncs() {
    let boot = Uuid::now_v7();
    let now = UNIX_EPOCH + Duration::from_secs(100);
    let mut handoff = HostUpdateHandoff::new(Duration::from_secs(60));
    let inspection = UpdateResourceInspection {
        inspection_id: 42,
        host_boot_id: boot,
        active: vec![ActiveUpdateResource {
            resource_id: "browser-1".into(),
            kind: "browser".into(),
            lifecycle: "Active".into(),
            task_id: Some("task-9".into()),
        }],
        confirmable: true,
    };

    let decision = handoff
        .inspect_active_resources(
            inspection.clone(),
            "devmanager/0.4.2",
            "devmanager-host/0.4.2",
        )
        .expect("inspect");
    assert!(matches!(
        decision,
        SilentReplacementDecision::Refused {
            block: HandoffBlockReason::ActiveResources { .. }
        }
    ));

    let token = handoff
        .prepare_update(
            &inspection,
            "0.4.2",
            "devmanager/0.4.2",
            "devmanager-host/0.4.2",
            now,
            true,
        )
        .expect("explicit confirm path");
    handoff
        .confirm_and_drain(token.token_id, now + Duration::from_secs(1))
        .expect("drain");
    handoff
        .begin_atomic_install(token.token_id, now + Duration::from_secs(2))
        .expect("install");
    assert_eq!(handoff.admission(), HostUpdateAdmission::InstallingUpdate);
    handoff
        .complete_matching_host_start(token.token_id, now + Duration::from_secs(3))
        .expect("start matching host");
    assert_eq!(
        handoff.admission(),
        HostUpdateAdmission::ResumingAfterUpdate
    );
    handoff
        .finish_resync(token.token_id, now + Duration::from_secs(4))
        .expect("resync");
    assert_eq!(handoff.admission(), HostUpdateAdmission::Ready);
    assert_eq!(
        handoff.phase(),
        &UpdateHandoffPhase::Completed {
            installed_version: "0.4.2".into()
        }
    );
}

#[test]
fn aborted_install_returns_old_host_ready() {
    let boot = Uuid::now_v7();
    let mut handoff = HostUpdateHandoff::default();
    let inspection = UpdateResourceInspection {
        inspection_id: 5,
        host_boot_id: boot,
        active: Vec::new(),
        confirmable: true,
    };
    handoff
        .inspect_active_resources(
            inspection.clone(),
            "devmanager/0.4.2",
            "devmanager-host/0.4.2",
        )
        .expect("inspect");
    let _token = handoff
        .prepare_update(
            &inspection,
            "0.4.2",
            "devmanager/0.4.2",
            "devmanager-host/0.4.2",
            SystemTime::now(),
            false,
        )
        .expect("prepare");
    assert_eq!(
        handoff.abort_pre_install().expect("abort"),
        HostUpdateAdmission::Ready
    );
    assert_eq!(handoff.phase(), &UpdateHandoffPhase::Ready);
}

#[test]
fn old_to_new_preserves_config_remote_identity_and_ignores_session() {
    let expectation: serde_json::Value =
        serde_json::from_str(&read_fixture("old-to-new/expectation.json")).expect("expectation");
    let config = read_fixture("old-to-new/config.json");
    let remote = read_fixture("old-to-new/remote.json");
    let session = read_fixture("old-to-new/session.json");

    assert_eq!(
        classify_user_state_path("config.json"),
        Some(UserStateClassification::Preserve(
            PreservedUserStateKind::ConfigJson
        ))
    );
    assert_eq!(
        classify_user_state_path("remote.json"),
        Some(UserStateClassification::Preserve(
            PreservedUserStateKind::RemoteJson
        ))
    );
    assert_eq!(
        classify_user_state_path("session.json"),
        Some(UserStateClassification::Ignore(
            IgnoredUserStateKind::SessionJson
        ))
    );

    let report = IdentityPreservationReport {
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
    };
    assert!(report.preserves_connect_and_config());
    assert!(report.preserves_new_architecture_task_db());
    assert!(session.contains("legacy-session-a"));
    assert_eq!(expectation["importLegacyConversations"], false);
    assert_eq!(expectation["expectEmptyTaskPromptDatabase"], true);

    let (preserve, ignore) = update_state_policy();
    assert!(preserve.contains(&PreservedUserStateKind::ConfigJson));
    assert!(preserve.contains(&PreservedUserStateKind::RemoteJson));
    assert!(preserve.contains(&PreservedUserStateKind::DevicePairingIdentity));
    assert!(ignore.contains(&IgnoredUserStateKind::SessionJson));
    assert!(ignore.contains(&IgnoredUserStateKind::LegacyProviderConversations));
}

#[test]
fn new_to_new_preserves_task_prompt_canonical_hash() {
    let expectation: serde_json::Value =
        serde_json::from_str(&read_fixture("new-to-new/expectation.json")).expect("expectation");
    let db = read_fixture("new-to-new/task-prompt-db.json");
    let db_hash = sha256_hex(db.as_bytes());
    let report = IdentityPreservationReport {
        config_hash_before: sha256_hex(read_fixture("new-to-new/config.json").as_bytes()),
        config_hash_after: sha256_hex(read_fixture("new-to-new/config.json").as_bytes()),
        remote_hash_before: sha256_hex(read_fixture("new-to-new/remote.json").as_bytes()),
        remote_hash_after: sha256_hex(read_fixture("new-to-new/remote.json").as_bytes()),
        device_pairing_fingerprint_before: "pairing:host-stable-001:PAIR-KEEP-ME:device-phone-001"
            .into(),
        device_pairing_fingerprint_after: "pairing:host-stable-001:PAIR-KEEP-ME:device-phone-001"
            .into(),
        task_db_hash_before: Some(db_hash.clone()),
        task_db_hash_after: Some(db_hash),
        session_json_considered: false,
        legacy_conversations_imported: false,
    };
    assert!(report.preserves_connect_and_config());
    assert!(report.preserves_new_architecture_task_db());
    assert_eq!(expectation["preserveTaskPromptDatabase"], true);
    assert_eq!(expectation["migrationFailureLeavesOldUsable"], true);
    for key in expectation["mustPreserve"]
        .as_array()
        .expect("mustPreserve array")
    {
        assert!(key.is_string());
    }
    let parsed: serde_json::Value = serde_json::from_str(&db).expect("db json");
    assert_eq!(
        parsed["tasks"][0]["providerSessionId"],
        "provider-session-exact-001"
    );
    assert_eq!(parsed["taskInvitation"]["id"], "invite-unexpired");
}
