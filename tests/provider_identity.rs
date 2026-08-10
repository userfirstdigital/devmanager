use devmanager::domain::agent::{
    AgentRole, AgentSessionFacts, ProviderSessionId, ProviderSessionIdError,
    MAX_PROVIDER_SESSION_ID_BYTES,
};
use devmanager::domain::id::TaskId;
use devmanager::providers::{
    CapabilityEvidence, CapabilityEvidenceError, CapabilitySupport, EvidenceConfidence,
    EvidenceSourceId, EvidenceStatus, ProviderAuthEvidenceRegistry, ProviderAuthEvidenceSource,
    ProviderAuthProbeResult, ProviderAuthState, ProviderCapabilities,
    ProviderDiscoveryCandidateInput, ProviderDiscoveryContract, ProviderDiscoveryError,
    ProviderDiscoveryOrigin, ProviderExecutable, ProviderExecutableError, ProviderExecutableForm,
    ProviderKind, ProviderPathSnapshot,
};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn fixture_file(root: &Path, name: &str, contents: &[u8]) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, contents).expect("write fixture file");
    path
}

fn native_fixture(root: &Path, name: &str, marker: &[u8]) -> PathBuf {
    // Test-only identity material; this is never treated as a stock provider
    // executable or granted production launch/auth authority.
    let path = root.join(name);
    fs::copy(
        std::env::current_exe().expect("current test executable"),
        &path,
    )
    .expect("copy current platform executable");
    if !marker.is_empty() {
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open copied executable")
            .write_all(marker)
            .expect("append fixture marker");
    }
    path
}

fn replace_with_native_fixture(path: &Path, marker: &[u8]) {
    let replacement = path.with_extension("replacement");
    native_fixture(
        path.parent().expect("fixture parent"),
        replacement.file_name().unwrap().to_str().unwrap(),
        marker,
    );
    fs::remove_file(path).expect("remove replaced path");
    fs::rename(replacement, path).expect("install replacement executable");
}

fn file_hash(path: &Path) -> [u8; 32] {
    Sha256::digest(fs::read(path).expect("read fixture file")).into()
}

fn executable(path: &Path) -> ProviderExecutable {
    ProviderExecutable::from_path(path).expect("strict executable identity")
}

fn stable_capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        kind: ProviderKind::ClaudeCode,
        version: devmanager::providers::ProviderVersion::new("fixture-1").unwrap(),
        auth_state: ProviderAuthState::Unknown,
        exact_resume: CapabilitySupport::Supported,
        semantic_events: CapabilitySupport::Unknown,
        provider_session_id: CapabilitySupport::Supported,
        build_launch: CapabilitySupport::Unknown,
        parse_signal: CapabilitySupport::Unknown,
        cooperative_stop: CapabilitySupport::Unknown,
        observe_quota: CapabilitySupport::Unknown,
        evidence: vec![CapabilityEvidence::new(
            EvidenceSourceId::Registry,
            1,
            EvidenceStatus::Unknown,
            None,
        )
        .unwrap()],
    }
}

#[test]
fn provider_session_id_preserves_exact_bytes_through_serde_and_sql() {
    let exact = "Provider/Session+identity_日本";
    let id = ProviderSessionId::new(exact.to_string()).expect("valid exact id");

    let encoded = serde_json::to_string(&id).expect("serialize provider id");
    let decoded: ProviderSessionId =
        serde_json::from_str(&encoded).expect("deserialize provider id");
    assert_eq!(decoded.as_bytes(), exact.as_bytes());

    let connection = Connection::open_in_memory().expect("open sqlite");
    connection
        .execute("CREATE TABLE ids (value TEXT NOT NULL)", [])
        .expect("create ids table");
    connection
        .execute("INSERT INTO ids (value) VALUES (?)", params![&id])
        .expect("insert provider id");
    let stored: ProviderSessionId = connection
        .query_row("SELECT value FROM ids", [], |row| row.get(0))
        .expect("read checked provider id");
    assert_eq!(stored.as_bytes(), exact.as_bytes());

    assert!(matches!(
        ProviderSessionId::new(String::new()),
        Err(ProviderSessionIdError::Empty)
    ));
    assert!(ProviderSessionId::new("provider\nidentity").is_err());
    assert!(ProviderSessionId::new("provider\u{202e}identity").is_err());
    assert!(ProviderSessionId::new("x".repeat(MAX_PROVIDER_SESSION_ID_BYTES + 1)).is_err());
}

#[test]
fn agent_session_facts_keep_provider_identity_typed_and_exact() {
    let exact = ProviderSessionId::new("provider-id".to_owned()).unwrap();
    let facts = AgentSessionFacts::new(
        TaskId::new(),
        AgentRole::Primary,
        ProviderKind::ClaudeCode,
        Some(exact.clone()),
    )
    .unwrap();

    assert_eq!(facts.provider_session_id.as_ref(), Some(&exact));
    assert_eq!(
        facts.provider_session_id.unwrap().as_bytes(),
        b"provider-id"
    );
}

#[test]
fn executable_identity_is_canonical_file_bound_and_checked_on_serde() {
    let temp = tempdir().unwrap();
    let path = native_fixture(temp.path(), "provider-native.exe", b"provider-a");
    let identity = executable(&path);

    assert!(identity.canonical_path().is_absolute());
    assert_eq!(identity.sha256(), &file_hash(&path));
    assert_eq!(identity.file_identity().link_count(), 1);
    assert!(identity.file_identity().stable_id() != 0);

    let encoded = serde_json::to_value(&identity).unwrap();
    assert!(encoded.get("file_identity").is_some());
    let decoded: ProviderExecutable = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(decoded, identity);

    let mut forged = encoded;
    forged["sha256"] = serde_json::json!(vec![0_u8; 32]);
    assert!(serde_json::from_value::<ProviderExecutable>(forged).is_err());
}

#[test]
fn executable_identity_rejects_replacement_even_when_the_path_is_unchanged() {
    let temp = tempdir().unwrap();
    let path = native_fixture(temp.path(), "provider-native.exe", b"provider-a");
    let original = executable(&path);

    replace_with_native_fixture(&path, b"provider-b");
    assert!(original.validate_current().is_err());
    let replacement = executable(&path);
    assert_ne!(replacement, original);
}

#[test]
fn plaintext_provider_name_is_not_a_runnable_native_executable() {
    let temp = tempdir().unwrap();
    let name = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    let path = fixture_file(temp.path(), name, b"not a native executable");

    assert!(matches!(
        ProviderExecutable::from_path(path),
        Err(ProviderExecutableError::NotNativeExecutable(_))
    ));
}

#[test]
fn launch_handle_retains_file_identity_and_rejects_path_replacement() {
    let temp = tempdir().unwrap();
    let name = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    let path = native_fixture(temp.path(), name, b"original");
    let identity = executable(&path);
    let handle = identity
        .open_for_launch()
        .expect("open trusted launch handle");

    assert_eq!(handle.file_identity(), identity.file_identity());
    handle
        .revalidate()
        .expect("captured handle is initially current");
    let consumed = handle
        .clone()
        .into_file()
        .expect("launcher can consume the validated handle");
    assert_eq!(
        consumed.metadata().unwrap().len(),
        fs::metadata(&path).unwrap().len()
    );

    replace_with_native_fixture(&path, b"replacement");

    assert!(handle.revalidate().is_err());
    assert!(identity.validate_current().is_err());
}

#[test]
fn executable_identity_rejects_directories_symlinks_and_hardlinks_when_supported() {
    let temp = tempdir().unwrap();
    let directory = temp.path().join("provider-native.exe");
    fs::create_dir(&directory).unwrap();
    assert!(ProviderExecutable::from_path(&directory).is_err());

    let target = native_fixture(temp.path(), "target.exe", b"provider");
    let symlink = temp.path().join("provider-link.exe");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, &symlink).unwrap();
        assert!(ProviderExecutable::from_path(&symlink).is_err());
    }
    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_file(&target, &symlink).is_ok() {
            assert!(ProviderExecutable::from_path(&symlink).is_err());
        }
    }

    let hardlink = temp.path().join("provider-hardlink.exe");
    #[cfg(unix)]
    {
        std::fs::hard_link(&target, &hardlink).unwrap();
        assert!(ProviderExecutable::from_path(&target).is_err());
        assert!(ProviderExecutable::from_path(&hardlink).is_err());
    }
    #[cfg(windows)]
    {
        if std::fs::hard_link(&target, &hardlink).is_ok() {
            assert!(ProviderExecutable::from_path(&target).is_err());
            assert!(ProviderExecutable::from_path(&hardlink).is_err());
        }
    }
}

#[test]
fn discovery_preserves_order_and_provenance_and_rejects_desktop_cursor() {
    let temp = tempdir().unwrap();
    let native_name = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    let path = native_fixture(temp.path(), native_name, b"controlled-native");
    let contract = ProviderDiscoveryContract::for_kind(ProviderKind::ClaudeCode);
    let path_value = std::env::join_paths([temp.path().as_os_str()]).unwrap();
    let snapshot = ProviderPathSnapshot::capture(&OsString::from(path_value)).unwrap();
    let mut candidates = contract.resolve_all_from_path_snapshot(&snapshot).unwrap();
    candidates.push(
        contract
            .validate(ProviderDiscoveryCandidateInput::configured_override(
                path.clone(),
            ))
            .unwrap(),
    );

    assert_eq!(candidates.len(), 2);
    assert!(matches!(
        candidates[0].origin(),
        ProviderDiscoveryOrigin::PathEntry { index: 0, .. }
    ));
    assert!(matches!(
        candidates[1].origin(),
        ProviderDiscoveryOrigin::ConfiguredOverride
    ));
    assert!(matches!(
        candidates[0].form(),
        ProviderExecutableForm::Native
    ));
    assert!(contract
        .validate(ProviderDiscoveryCandidateInput::Native {
            path: path.clone(),
            origin: ProviderDiscoveryOrigin::PathEntry {
                index: 99,
                directory: temp.path().to_path_buf(),
            },
        })
        .is_err());

    let cursor_name = if cfg!(windows) {
        "cursor.exe"
    } else {
        "cursor"
    };
    let cursor = native_fixture(temp.path(), cursor_name, b"desktop-cursor");
    let cursor_contract = ProviderDiscoveryContract::for_kind(ProviderKind::Cursor);
    assert!(cursor_contract
        .validate(ProviderDiscoveryCandidateInput::configured_override(cursor))
        .is_err());
}

#[test]
fn path_snapshot_resolver_owns_path_order_and_provenance() {
    let temp = tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    native_fixture(
        &first,
        if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        },
        b"first",
    );
    native_fixture(
        &second,
        if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        },
        b"second",
    );

    let path = std::env::join_paths([first.as_os_str(), second.as_os_str()]).unwrap();
    let snapshot = ProviderPathSnapshot::capture(&OsString::from(path)).unwrap();
    let contract = ProviderDiscoveryContract::for_kind(ProviderKind::ClaudeCode);
    let candidates = contract
        .resolve_all_from_path_snapshot(&snapshot)
        .expect("resolve captured PATH snapshot");

    assert_eq!(candidates.len(), 2);
    assert!(matches!(
        candidates[0].origin(),
        ProviderDiscoveryOrigin::PathEntry { index: 0, .. }
    ));
    assert_eq!(
        candidates[0].executable().canonical_path().parent(),
        Some(fs::canonicalize(&first).unwrap().as_path())
    );
    assert_ne!(
        candidates[0].executable().file_identity(),
        candidates[1].executable().file_identity()
    );
}

#[cfg(unix)]
#[test]
fn path_snapshot_rejects_reparse_directory_before_canonicalization() {
    use std::os::unix::fs::symlink;

    let temp = tempdir().unwrap();
    let real = temp.path().join("real");
    let link = temp.path().join("path-link");
    fs::create_dir(&real).unwrap();
    symlink(&real, &link).unwrap();
    let path_value = std::env::join_paths([link.as_os_str()]).unwrap();

    assert!(matches!(
        ProviderPathSnapshot::capture(&OsString::from(path_value)),
        Err(devmanager::providers::ProviderDiscoveryError::InvalidPathSnapshot(_))
    ));
}

#[test]
fn path_snapshot_resolver_fails_closed_on_a_shadowing_non_native_candidate() {
    let temp = tempdir().unwrap();
    let shadow = temp.path().join("shadow");
    let fallback = temp.path().join("fallback");
    fs::create_dir_all(&shadow).unwrap();
    fs::create_dir_all(&fallback).unwrap();
    let name = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    fixture_file(&shadow, name, b"shadowing text");
    native_fixture(&fallback, name, b"fallback");

    let path = std::env::join_paths([shadow.as_os_str(), fallback.as_os_str()]).unwrap();
    let snapshot = ProviderPathSnapshot::capture(&OsString::from(path)).unwrap();
    let contract = ProviderDiscoveryContract::for_kind(ProviderKind::ClaudeCode);

    assert!(matches!(
        contract.resolve_from_path_snapshot(&snapshot),
        Err(devmanager::providers::ProviderDiscoveryError::Executable(
            ProviderExecutableError::NotNativeExecutable(_)
        ))
    ));
}

#[cfg(windows)]
#[test]
fn discovery_accepts_only_the_controlled_runnable_windows_shim_contract() {
    let temp = tempdir().unwrap();
    let fixture_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/providers/registry");
    let target = temp.path().join("claude.exe");
    let shim = temp.path().join("claude.cmd");
    fs::copy(std::env::current_exe().unwrap(), &target).unwrap();
    fs::copy(fixture_root.join("identity_claude.cmd"), &shim).unwrap();
    let contract = ProviderDiscoveryContract::for_kind(ProviderKind::ClaudeCode);
    let path_value = std::env::join_paths([temp.path().as_os_str()]).unwrap();
    let snapshot = ProviderPathSnapshot::capture(&OsString::from(path_value)).unwrap();
    let candidates = contract.resolve_all_from_path_snapshot(&snapshot).unwrap();
    assert!(candidates
        .iter()
        .any(|candidate| matches!(candidate.form(), ProviderExecutableForm::WindowsShim { .. })));

    let arbitrary = fixture_file(temp.path(), "arbitrary.cmd", b"echo provider");
    assert!(contract
        .validate(ProviderDiscoveryCandidateInput::configured_override(
            arbitrary
        ))
        .is_err());
}

#[test]
fn auth_evidence_is_registry_issued_fresh_and_bound_to_identity() {
    let temp = tempdir().unwrap();
    let path = native_fixture(temp.path(), "provider-native.exe", b"provider-a");
    let replacement_path = native_fixture(temp.path(), "provider-other.exe", b"provider-b");
    let identity = executable(&path);
    let replacement = executable(&replacement_path);
    let issued_at = Instant::now();
    let deadline = issued_at + Duration::from_secs(30);
    let mut evidence = ProviderAuthEvidenceRegistry::new();
    let invocation = evidence
        .begin_at(
            ProviderKind::ClaudeCode,
            identity.clone(),
            issued_at,
            deadline,
        )
        .unwrap();

    let observed_at = Instant::now();
    let receipt = evidence
        .accept_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            invocation.clone(),
            ProviderAuthProbeResult::AuthRequired,
            observed_at,
        )
        .unwrap();
    assert!(receipt.is_fresh_at(observed_at + Duration::from_secs(1)));
    assert!(!receipt.is_authenticated_subscription());
    assert_eq!(
        receipt.source(),
        ProviderAuthEvidenceSource::ClaudeCodeSubscriptionLogin
    );
    assert_eq!(receipt.confidence(), EvidenceConfidence::High);
    assert!(receipt.expires_at() > receipt.observed_at());
    assert!(evidence
        .accept_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            invocation.clone(),
            ProviderAuthProbeResult::AuthRequired,
            Instant::now(),
        )
        .is_err());

    let wrong_identity = evidence
        .begin_at(
            ProviderKind::ClaudeCode,
            identity.clone(),
            issued_at,
            deadline,
        )
        .unwrap();
    assert!(evidence
        .accept_at_for(
            ProviderKind::Codex,
            &replacement,
            wrong_identity,
            ProviderAuthProbeResult::AuthRequired,
            Instant::now(),
        )
        .is_err());
}

#[test]
fn public_auth_acceptance_cannot_forge_authenticated_subscription_or_extend_time() {
    let temp = tempdir().unwrap();
    let path = native_fixture(temp.path(), "provider-native.exe", b"provider-a");
    let identity = executable(&path);
    let mut evidence = ProviderAuthEvidenceRegistry::new();
    let invocation = evidence
        .begin(
            ProviderKind::ClaudeCode,
            identity.clone(),
            Duration::from_secs(30),
        )
        .unwrap();

    assert!(evidence
        .accept_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            invocation,
            ProviderAuthProbeResult::AuthenticatedSubscription,
            Instant::now(),
        )
        .is_err());
}

#[test]
fn auth_evidence_rejects_expired_reordered_same_timestamp_and_api_key_claims() {
    let temp = tempdir().unwrap();
    let path = native_fixture(temp.path(), "provider-native.exe", b"provider-a");
    let identity = executable(&path);
    let now = Instant::now();
    let mut evidence = ProviderAuthEvidenceRegistry::new();

    let expired = evidence
        .begin(
            ProviderKind::ClaudeCode,
            identity.clone(),
            Duration::from_millis(1),
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(5));
    assert!(evidence
        .accept_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            expired,
            ProviderAuthProbeResult::Unknown,
            now,
        )
        .is_err());

    let first = evidence
        .begin_at(
            ProviderKind::ClaudeCode,
            identity.clone(),
            Instant::now(),
            now + Duration::from_secs(30),
        )
        .unwrap();
    let second = evidence
        .begin_at(
            ProviderKind::ClaudeCode,
            identity.clone(),
            Instant::now(),
            now + Duration::from_secs(30),
        )
        .unwrap();
    let later = Instant::now();
    evidence
        .accept_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            second.clone(),
            ProviderAuthProbeResult::Unknown,
            later,
        )
        .unwrap();
    assert!(evidence
        .accept_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            first,
            ProviderAuthProbeResult::Unknown,
            Instant::now(),
        )
        .is_err());

    let same_timestamp_a = evidence
        .begin_at(
            ProviderKind::ClaudeCode,
            identity.clone(),
            Instant::now(),
            later + Duration::from_secs(30),
        )
        .unwrap();
    let same_timestamp_b = evidence
        .begin_at(
            ProviderKind::ClaudeCode,
            identity.clone(),
            Instant::now(),
            later + Duration::from_secs(30),
        )
        .unwrap();
    let same_observed_at = Instant::now();
    evidence
        .accept_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            same_timestamp_a,
            ProviderAuthProbeResult::Unknown,
            same_observed_at,
        )
        .unwrap();
    assert!(evidence
        .accept_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            same_timestamp_b,
            ProviderAuthProbeResult::Unknown,
            same_observed_at,
        )
        .is_ok());

    let api_key = evidence
        .begin_at(
            ProviderKind::ClaudeCode,
            identity.clone(),
            Instant::now(),
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
    let api_key_receipt = evidence
        .accept_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            api_key,
            ProviderAuthProbeResult::ApiKeyDetected,
            Instant::now(),
        )
        .unwrap();
    assert!(!api_key_receipt.is_authenticated_subscription());
    assert_eq!(
        api_key_receipt.source(),
        ProviderAuthEvidenceSource::ClaudeCodeSubscriptionLogin
    );
    assert_eq!(api_key_receipt.confidence(), EvidenceConfidence::Low);
}

#[test]
fn auth_receipt_consumption_is_one_shot_fresh_and_identity_bound() {
    let temp = tempdir().unwrap();
    let path = native_fixture(temp.path(), "provider-native.exe", b"provider-a");
    let replacement_path = native_fixture(temp.path(), "provider-other.exe", b"provider-b");
    let identity = executable(&path);
    let other_identity = executable(&replacement_path);

    let mut evidence = ProviderAuthEvidenceRegistry::new();
    let issued_at = Instant::now() - Duration::from_secs(1);
    let invocation = evidence
        .begin_at(
            ProviderKind::ClaudeCode,
            identity.clone(),
            issued_at,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
    let receipt = evidence
        .accept_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            invocation,
            ProviderAuthProbeResult::AuthRequired,
            Instant::now(),
        )
        .unwrap();
    assert!(evidence
        .consume_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            receipt.clone(),
            Instant::now(),
        )
        .is_ok());
    assert!(matches!(
        evidence.consume_at_for(ProviderKind::ClaudeCode, &identity, receipt, Instant::now(),),
        Err(devmanager::providers::ProviderAuthEvidenceError::AlreadyConsumed)
    ));

    let mut stale_registry = ProviderAuthEvidenceRegistry::new();
    let stale_invocation = stale_registry
        .begin_at(
            ProviderKind::ClaudeCode,
            identity.clone(),
            Instant::now() - Duration::from_secs(1),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    let stale_receipt = stale_registry
        .accept_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            stale_invocation,
            ProviderAuthProbeResult::AuthRequired,
            Instant::now(),
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(2_100));
    assert!(matches!(
        stale_registry.consume_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            stale_receipt,
            Instant::now() + Duration::from_secs(1),
        ),
        Err(devmanager::providers::ProviderAuthEvidenceError::Expired)
    ));

    let mut future_registry = ProviderAuthEvidenceRegistry::new();
    let future_issued_at = Instant::now();
    let future_invocation = future_registry
        .begin_at(
            ProviderKind::ClaudeCode,
            identity.clone(),
            future_issued_at,
            future_issued_at + Duration::from_secs(30),
        )
        .unwrap();
    let future_receipt = future_registry
        .accept_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            future_invocation,
            ProviderAuthProbeResult::AuthRequired,
            future_issued_at + Duration::from_secs(1),
        )
        .unwrap();
    assert!(future_receipt.observed_at() < future_issued_at + Duration::from_secs(1));

    let mut ordering_registry = ProviderAuthEvidenceRegistry::new();
    let first = ordering_registry
        .begin_at(
            ProviderKind::ClaudeCode,
            identity.clone(),
            issued_at,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
    let first_receipt = ordering_registry
        .accept_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            first,
            ProviderAuthProbeResult::AuthRequired,
            Instant::now(),
        )
        .unwrap();
    let second = ordering_registry
        .begin_at(
            ProviderKind::ClaudeCode,
            identity.clone(),
            issued_at,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
    let second_receipt = ordering_registry
        .accept_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            second,
            ProviderAuthProbeResult::AuthRequired,
            first_receipt.observed_at() + Duration::from_nanos(1),
        )
        .unwrap();
    assert!(matches!(
        ordering_registry.consume_at_for(
            ProviderKind::Codex,
            &identity,
            first_receipt.clone(),
            Instant::now(),
        ),
        Err(devmanager::providers::ProviderAuthEvidenceError::WrongProvider)
    ));
    assert!(matches!(
        ordering_registry.consume_at_for(
            ProviderKind::ClaudeCode,
            &other_identity,
            first_receipt.clone(),
            Instant::now(),
        ),
        Err(devmanager::providers::ProviderAuthEvidenceError::WrongExecutable)
    ));
    assert!(matches!(
        ordering_registry.consume_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            first_receipt,
            Instant::now(),
        ),
        Err(devmanager::providers::ProviderAuthEvidenceError::Reordered)
    ));
    assert!(ordering_registry
        .consume_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            second_receipt,
            Instant::now(),
        )
        .is_ok());

    let mut replacement_registry = ProviderAuthEvidenceRegistry::new();
    let replacement_invocation = replacement_registry
        .begin(
            ProviderKind::ClaudeCode,
            identity.clone(),
            Duration::from_secs(30),
        )
        .unwrap();
    let replacement_receipt = replacement_registry
        .accept_now(
            replacement_invocation,
            ProviderAuthProbeResult::AuthRequired,
        )
        .unwrap();
    replace_with_native_fixture(&path, b"provider-replaced");
    assert!(matches!(
        replacement_registry.consume_at_for(
            ProviderKind::ClaudeCode,
            &identity,
            replacement_receipt,
            Instant::now(),
        ),
        Err(devmanager::providers::ProviderAuthEvidenceError::ExecutableChanged(_))
    ));
}

#[test]
fn auth_pending_and_accepted_receipts_have_deterministic_bounds() {
    let temp = tempdir().unwrap();
    let path = native_fixture(temp.path(), "provider-native.exe", b"provider-a");
    let identity = executable(&path);

    let mut pending = ProviderAuthEvidenceRegistry::new();
    for _ in 0..(devmanager::providers::MAX_PROVIDER_AUTH_PENDING_ENTRIES + 16) {
        pending
            .begin(
                ProviderKind::ClaudeCode,
                identity.clone(),
                Duration::from_secs(240),
            )
            .unwrap();
    }
    assert_eq!(
        pending.pending_len(),
        devmanager::providers::MAX_PROVIDER_AUTH_PENDING_ENTRIES
    );

    let mut accepted = ProviderAuthEvidenceRegistry::new();
    for _ in 0..(devmanager::providers::MAX_PROVIDER_AUTH_ACCEPTED_ENTRIES + 16) {
        let invocation = accepted
            .begin(
                ProviderKind::ClaudeCode,
                identity.clone(),
                Duration::from_secs(240),
            )
            .unwrap();
        accepted
            .accept_now(invocation, ProviderAuthProbeResult::Unknown)
            .unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        accepted.accepted_len(),
        devmanager::providers::MAX_PROVIDER_AUTH_ACCEPTED_ENTRIES
    );
}

#[test]
fn cache_hit_auth_observation_is_fresh_and_correlated_without_mutating_stable_cache() {
    let temp = tempdir().unwrap();
    let path = native_fixture(temp.path(), "provider-native.exe", b"provider-a");
    let identity = executable(&path);
    let capabilities = stable_capabilities();
    let stable = capabilities.stable_projection();
    let mut evidence = ProviderAuthEvidenceRegistry::new();
    let first = evidence
        .begin(
            ProviderKind::ClaudeCode,
            identity.clone(),
            Duration::from_secs(30),
        )
        .unwrap();
    let first_receipt = evidence
        .accept_now(first, ProviderAuthProbeResult::AuthRequired)
        .unwrap();
    let second = evidence
        .begin(
            ProviderKind::ClaudeCode,
            identity.clone(),
            Duration::from_secs(30),
        )
        .unwrap();
    let second_receipt = evidence
        .accept_now(second, ProviderAuthProbeResult::AuthRequired)
        .unwrap();

    assert!(second_receipt.generation() > first_receipt.generation());
    assert_eq!(second_receipt.executable(), &identity);
    assert!(second_receipt.is_fresh_at(Instant::now()));
    assert_eq!(
        stable.auth_state(),
        devmanager::providers::ProviderAuthState::Unknown
    );
    assert!(stable
        .evidence()
        .iter()
        .all(|evidence| evidence.source() != EvidenceSourceId::AuthStatusProbe));
}

#[test]
fn stable_capabilities_projection_does_not_retain_auth_evidence() {
    let capabilities = stable_capabilities();
    let stable = capabilities.stable_projection();

    assert_eq!(
        stable.auth_state(),
        devmanager::providers::ProviderAuthState::Unknown
    );
    assert!(stable
        .evidence()
        .iter()
        .all(|evidence| evidence.source()
            != devmanager::providers::EvidenceSourceId::AuthStatusProbe));
}

#[test]
fn provider_capability_wire_rejects_forged_auth_and_requires_all_fields() {
    let mut wire: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/providers/registry/authenticated_subscription.json"
    ))
    .unwrap();
    wire["schema_version"] = serde_json::json!(1);
    wire["evidence"][0]["auth_source"] = serde_json::json!("claude_code_subscription_login");
    wire["evidence"][0]["expires_at"] = serde_json::json!(1_700_000_036_000_u64);
    wire["evidence"][0]["confidence"] = serde_json::json!("high");

    assert!(serde_json::from_value::<ProviderCapabilities>(wire).is_err());

    let encoded = serde_json::to_value(stable_capabilities()).unwrap();

    assert_eq!(encoded["schema_version"], 1);
    assert_eq!(encoded["evidence"][0]["source"], "registry");

    let mut missing_required = encoded.clone();
    missing_required
        .as_object_mut()
        .unwrap()
        .remove("build_launch");
    assert!(serde_json::from_value::<ProviderCapabilities>(missing_required).is_err());

    let mut future = encoded.clone();
    future["schema_version"] = serde_json::json!(2);
    assert!(serde_json::from_value::<ProviderCapabilities>(future).is_err());

    let mut unknown = encoded;
    unknown["unrecognized"] = serde_json::json!(true);
    assert!(serde_json::from_value::<ProviderCapabilities>(unknown).is_err());

    let duplicate = r#"{
        "schema_version":1,"kind":"claude_code","version":"fixture-1",
        "auth_state":"unknown","exact_resume":"supported","semantic_events":"unknown",
        "provider_session_id":"supported","build_launch":"unknown","build_launch":"unknown",
        "parse_signal":"unknown","cooperative_stop":"unknown","observe_quota":"unknown",
        "evidence":[]
    }"#;
    assert!(serde_json::from_str::<ProviderCapabilities>(duplicate).is_err());
}

#[test]
fn provider_executable_wire_is_versioned_and_rejects_oversized_paths() {
    let temp = tempdir().unwrap();
    let path = native_fixture(temp.path(), "provider-native.exe", b"versioned");
    let identity = executable(&path);
    let encoded = serde_json::to_value(&identity).unwrap();

    assert_eq!(encoded["schema_version"], 1);

    let mut future = encoded.clone();
    future["schema_version"] = serde_json::json!(2);
    assert!(serde_json::from_value::<ProviderExecutable>(future).is_err());

    let mut oversized = encoded;
    oversized["canonical_path"] = serde_json::json!("x".repeat(16 * 1024));
    assert!(serde_json::from_value::<ProviderExecutable>(oversized).is_err());
}

#[test]
fn provider_identity_debug_and_errors_redact_paths_and_auth_nonces() {
    let temp = tempdir().unwrap();
    let secret = "provider-secret-token";
    let identity_path = native_fixture(temp.path(), &format!("{secret}.exe"), b"redact");
    let identity = executable(&identity_path);

    let identity_debug = format!("{identity:?}");
    assert!(!identity_debug.contains(secret));

    let plaintext = temp.path().join(format!("{secret}-plaintext.exe"));
    fs::write(&plaintext, b"not a native executable").unwrap();
    let error = ProviderExecutable::from_path(&plaintext).unwrap_err();
    assert!(!format!("{error:?}").contains(secret));
    assert!(!error.to_string().contains(secret));

    let mut evidence = ProviderAuthEvidenceRegistry::new();
    let invocation = evidence
        .begin(ProviderKind::ClaudeCode, identity, Duration::from_secs(30))
        .unwrap();
    let nonce_debug = format!("{:?}", invocation.nonce());
    let invocation_debug = format!("{invocation:?}");
    assert!(!invocation_debug.contains(secret));
    assert!(!invocation_debug.contains(&nonce_debug));

    let receipt = evidence
        .accept_now(invocation, ProviderAuthProbeResult::AuthRequired)
        .unwrap();
    let receipt_debug = format!("{receipt:?}");
    assert!(!receipt_debug.contains(secret));
    assert!(!receipt_debug.contains(&nonce_debug));
}

#[test]
fn provider_discovery_debug_and_errors_redact_paths() {
    let temp = tempdir().unwrap();
    let secret = "discovery-secret-token";
    let root = temp.path().join(secret);
    fs::create_dir(&root).unwrap();
    let invalid_path = native_fixture(
        &root,
        if cfg!(windows) {
            "not-claude.exe"
        } else {
            "not-claude"
        },
        b"invalid-entrypoint",
    );
    let contract = ProviderDiscoveryContract::for_kind(ProviderKind::ClaudeCode);
    let error = contract
        .validate(ProviderDiscoveryCandidateInput::configured_override(
            invalid_path.clone(),
        ))
        .unwrap_err();

    for rendered in [format!("{error:?}"), error.to_string()] {
        assert!(
            !rendered.contains(secret),
            "leaked discovery path: {rendered}"
        );
    }

    let input = ProviderDiscoveryCandidateInput::configured_override(invalid_path);
    assert!(!format!("{input:?}").contains(secret));

    let valid_path = native_fixture(
        &root,
        if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        },
        b"valid-entrypoint",
    );
    let candidate = contract
        .validate(ProviderDiscoveryCandidateInput::configured_override(
            valid_path,
        ))
        .unwrap();
    assert!(!format!("{candidate:?}").contains(secret));
    assert!(!format!("{:?}", candidate.origin()).contains(secret));

    let path_value = std::env::join_paths([root.as_os_str()]).unwrap();
    let snapshot = ProviderPathSnapshot::capture(&OsString::from(path_value)).unwrap();
    assert!(!format!("{snapshot:?}").contains(secret));

    let explicit = ProviderDiscoveryError::WrongEntrypoint(root.join("raw-secret.exe"));
    assert!(!format!("{explicit:?}").contains(secret));
    assert!(!explicit.to_string().contains(secret));
}

#[test]
fn generic_capability_evidence_cannot_authorize_subscription_auth() {
    assert!(matches!(
        CapabilityEvidence::new(
            EvidenceSourceId::AuthStatusProbe,
            1_700_000_000_000,
            EvidenceStatus::Authenticated,
            None,
        ),
        Err(CapabilityEvidenceError::AuthEvidenceRequiresReceipt)
    ));
    assert!(matches!(
        CapabilityEvidence::new(
            EvidenceSourceId::Registry,
            1_700_000_000_000,
            EvidenceStatus::Authenticated,
            None,
        ),
        Err(CapabilityEvidenceError::AuthEvidenceRequiresReceipt)
    ));

    let forged_wire = serde_json::json!({
        "schema_version": 1,
        "source": "auth_status_probe",
        "observed_at": 1_700_000_000_000u64,
        "expires_at": 1_700_000_000_100u64,
        "confidence": "high",
        "auth_source": "claude_code_subscription_login",
        "status": "authenticated",
        "diagnostic": null
    });
    assert!(serde_json::from_value::<CapabilityEvidence>(forged_wire).is_err());
}

#[test]
fn provider_wires_require_explicit_schema_and_reject_nested_unknown_fields() {
    let capability_fixture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/providers/registry/authenticated_subscription.json"
    ))
    .unwrap();
    assert!(serde_json::from_value::<ProviderCapabilities>(capability_fixture.clone()).is_err());

    let mut missing_capability_field = capability_fixture.clone();
    missing_capability_field["schema_version"] = serde_json::json!(1);
    missing_capability_field
        .as_object_mut()
        .unwrap()
        .remove("build_launch");
    assert!(serde_json::from_value::<ProviderCapabilities>(missing_capability_field).is_err());

    let temp = tempdir().unwrap();
    let path = native_fixture(temp.path(), "provider-native.exe", b"wire");
    let identity = executable(&path);
    let encoded_identity = serde_json::to_value(&identity).unwrap();

    let mut missing_executable_schema = encoded_identity.clone();
    missing_executable_schema
        .as_object_mut()
        .unwrap()
        .remove("schema_version");
    assert!(serde_json::from_value::<ProviderExecutable>(missing_executable_schema).is_err());

    let mut unknown_nested_identity = encoded_identity;
    unknown_nested_identity["file_identity"]["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<ProviderExecutable>(unknown_nested_identity).is_err());

    let evidence =
        CapabilityEvidence::new(EvidenceSourceId::Registry, 1, EvidenceStatus::Unknown, None)
            .unwrap();
    let mut missing_evidence_schema = serde_json::to_value(evidence).unwrap();
    missing_evidence_schema
        .as_object_mut()
        .unwrap()
        .remove("schema_version");
    assert!(serde_json::from_value::<CapabilityEvidence>(missing_evidence_schema).is_err());
}

#[test]
fn executable_wire_preserves_native_form_and_requires_it_on_decode() {
    let temp = tempdir().unwrap();
    let path = native_fixture(temp.path(), "provider-native.exe", b"native-form");
    let identity = executable(&path);
    let encoded = serde_json::to_value(&identity).unwrap();

    assert_eq!(encoded["is_native"], serde_json::json!(true));

    let mut missing_form = encoded;
    missing_form.as_object_mut().unwrap().remove("is_native");
    assert!(serde_json::from_value::<ProviderExecutable>(missing_form).is_err());
}

#[cfg(windows)]
#[test]
fn executable_wire_preserves_windows_shim_form() {
    let temp = tempdir().unwrap();
    let target = native_fixture(temp.path(), "claude.exe", b"shim-target");
    let shim = temp.path().join("claude.cmd");
    let fixture_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/providers/registry");
    fs::copy(fixture_root.join("identity_claude.cmd"), &shim).unwrap();
    let contract = ProviderDiscoveryContract::for_kind(ProviderKind::ClaudeCode);
    let candidate = contract
        .validate(ProviderDiscoveryCandidateInput::windows_shim(
            &shim,
            &target,
            ProviderDiscoveryOrigin::ConfiguredOverride,
        ))
        .unwrap();
    let encoded = serde_json::to_value(candidate.executable()).unwrap();
    let decoded: ProviderExecutable = serde_json::from_value(encoded).unwrap();
    assert!(!decoded.is_native());
    assert_eq!(decoded, *candidate.executable());
}

#[test]
fn agent_session_facts_accept_only_stock_provider_kinds() {
    assert!(ProviderKind::parse_wire("arbitrary-provider").is_none());

    let wire = serde_json::json!({
        "id": devmanager::domain::AgentSessionId::new(),
        "task_id": TaskId::new(),
        "role": "primary",
        "provider_kind": "arbitrary-provider",
        "provider_session_id": null,
        "lifecycle": "open",
        "runtime_generation": 0,
        "revision": 0
    });
    assert!(serde_json::from_value::<AgentSessionFacts>(wire).is_err());
}

#[test]
fn provider_executable_debug_redacts_path_file_name_and_content_hash() {
    let temp = tempdir().unwrap();
    let secret = "provider-secret-name";
    let path = native_fixture(temp.path(), format!("{secret}.exe").as_str(), &[0x5a; 32]);
    let identity = executable(&path);
    let rendered = format!("{identity:?}");

    assert!(!rendered.contains(secret));
    assert!(!rendered.contains(&identity.sha256_hex()));
    assert!(!rendered.contains("canonical_path"));
}
