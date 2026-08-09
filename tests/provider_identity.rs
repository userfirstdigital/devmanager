use devmanager::domain::agent::{
    AgentRole, AgentSessionFacts, ProviderSessionId, ProviderSessionIdError,
    MAX_PROVIDER_SESSION_ID_BYTES,
};
use devmanager::domain::id::TaskId;
use devmanager::providers::{
    EvidenceConfidence, ProviderAuthEvidenceRegistry, ProviderAuthEvidenceSource,
    ProviderAuthProbeResult, ProviderCapabilities, ProviderDiscoveryCandidateInput,
    ProviderDiscoveryContract, ProviderDiscoveryError, ProviderDiscoveryOrigin, ProviderExecutable,
    ProviderExecutableError, ProviderExecutableForm, ProviderKind, ProviderPathSnapshot,
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

#[test]
fn provider_session_id_preserves_exact_bytes_through_serde_and_sql() {
    let exact = "  Provider/Session+identity_日本  ";
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
    let exact = ProviderSessionId::new(" provider-id ".to_owned()).unwrap();
    let facts = AgentSessionFacts::new(
        TaskId::new(),
        AgentRole::Primary,
        "claude_code",
        Some(exact.clone()),
    )
    .unwrap();

    assert_eq!(facts.provider_session_id.as_ref(), Some(&exact));
    assert_eq!(
        facts.provider_session_id.unwrap().as_bytes(),
        b" provider-id "
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
            ProviderAuthProbeResult::AuthenticatedSubscription,
            observed_at,
        )
        .unwrap();
    assert!(receipt.is_fresh_at(observed_at + Duration::from_secs(1)));
    assert!(receipt.is_authenticated_subscription());
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
            ProviderAuthProbeResult::AuthenticatedSubscription,
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
        .begin_at(
            ProviderKind::ClaudeCode,
            identity.clone(),
            now - Duration::from_secs(3),
            now - Duration::from_secs(1),
        )
        .unwrap();
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
        .is_err());

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
fn cache_hit_auth_observation_is_fresh_and_correlated_without_mutating_stable_cache() {
    let temp = tempdir().unwrap();
    let path = native_fixture(temp.path(), "provider-native.exe", b"provider-a");
    let identity = executable(&path);
    let capabilities: ProviderCapabilities = serde_json::from_str(include_str!(
        "fixtures/providers/registry/authenticated_subscription.json"
    ))
    .unwrap();
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
        .accept_now(first, ProviderAuthProbeResult::AuthenticatedSubscription)
        .unwrap();
    let second = evidence
        .begin(
            ProviderKind::ClaudeCode,
            identity.clone(),
            Duration::from_secs(30),
        )
        .unwrap();
    let second_receipt = evidence
        .accept_now(second, ProviderAuthProbeResult::AuthenticatedSubscription)
        .unwrap();

    assert!(second_receipt.generation() > first_receipt.generation());
    assert_eq!(second_receipt.executable(), &identity);
    assert!(second_receipt.is_fresh_at(Instant::now()));
    assert_eq!(
        stable.auth_state(),
        devmanager::providers::ProviderAuthState::Unknown
    );
    assert!(stable.evidence().is_empty());
}

#[test]
fn stable_capabilities_projection_does_not_retain_auth_evidence() {
    let fixture = include_str!("fixtures/providers/registry/authenticated_subscription.json");
    let capabilities: ProviderCapabilities = serde_json::from_str(fixture).unwrap();
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
fn provider_capability_wire_is_versioned_and_keeps_auth_lifecycle_metadata() {
    let mut wire: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/providers/registry/authenticated_subscription.json"
    ))
    .unwrap();
    wire["schema_version"] = serde_json::json!(1);
    wire["evidence"][0]["auth_source"] = serde_json::json!("claude_code_subscription_login");
    wire["evidence"][0]["expires_at"] = serde_json::json!(1_700_000_036_000_u64);
    wire["evidence"][0]["confidence"] = serde_json::json!("high");

    let capabilities: ProviderCapabilities = serde_json::from_value(wire).unwrap();
    let encoded = serde_json::to_value(capabilities).unwrap();

    assert_eq!(encoded["schema_version"], 1);
    assert_eq!(
        encoded["evidence"][0]["auth_source"],
        "claude_code_subscription_login"
    );
    assert_eq!(encoded["evidence"][0]["expires_at"], 1_700_000_036_000_u64);
    assert_eq!(encoded["evidence"][0]["confidence"], "high");

    let mut wrong_source = encoded.clone();
    wrong_source["evidence"][0]["auth_source"] = serde_json::json!("codex_subscription_login");
    assert!(serde_json::from_value::<ProviderCapabilities>(wrong_source).is_err());

    let mut future = encoded.clone();
    future["schema_version"] = serde_json::json!(2);
    assert!(serde_json::from_value::<ProviderCapabilities>(future).is_err());

    let mut unknown = encoded;
    unknown["unrecognized"] = serde_json::json!(true);
    assert!(serde_json::from_value::<ProviderCapabilities>(unknown).is_err());
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
        .accept_now(
            invocation,
            ProviderAuthProbeResult::AuthenticatedSubscription,
        )
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
