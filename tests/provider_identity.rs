use devmanager::domain::agent::{
    AgentRole, AgentSessionFacts, ProviderSessionId, ProviderSessionIdError,
    MAX_PROVIDER_SESSION_ID_BYTES,
};
use devmanager::domain::id::TaskId;
use devmanager::providers::{
    ProviderAuthEvidenceRegistry, ProviderAuthProbeResult, ProviderCapabilities,
    ProviderDiscoveryCandidateInput, ProviderDiscoveryContract, ProviderDiscoveryOrigin,
    ProviderExecutable, ProviderExecutableForm, ProviderKind,
};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn fixture_file(root: &Path, name: &str, contents: &[u8]) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, contents).expect("write fixture file");
    path
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
    let path = fixture_file(temp.path(), "provider-native.exe", b"provider-a");
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
    let path = fixture_file(temp.path(), "provider-native.exe", b"provider-a");
    let original = executable(&path);

    fs::write(&path, b"provider-b").unwrap();
    assert!(original.validate_current().is_err());
    let replacement = executable(&path);
    assert_ne!(replacement, original);
}

#[test]
fn executable_identity_rejects_directories_symlinks_and_hardlinks_when_supported() {
    let temp = tempdir().unwrap();
    let directory = temp.path().join("provider-native.exe");
    fs::create_dir(&directory).unwrap();
    assert!(ProviderExecutable::from_path(&directory).is_err());

    let target = fixture_file(temp.path(), "target.exe", b"provider");
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
    let path = fixture_file(temp.path(), native_name, b"controlled-native");
    let contract = ProviderDiscoveryContract::for_kind(ProviderKind::ClaudeCode);
    let candidates = contract
        .validate_in_order([
            ProviderDiscoveryCandidateInput::path_entry(path.clone(), 3, temp.path().to_path_buf()),
            ProviderDiscoveryCandidateInput::configured_override(path.clone()),
        ])
        .unwrap();

    assert_eq!(candidates.len(), 2);
    assert!(matches!(
        candidates[0].origin(),
        ProviderDiscoveryOrigin::PathEntry { index: 3, .. }
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
        .validate(ProviderDiscoveryCandidateInput::path_entry(
            path.clone(),
            3,
            temp.path().parent().unwrap().to_path_buf(),
        ))
        .is_err());

    let cursor_name = if cfg!(windows) {
        "cursor.exe"
    } else {
        "cursor"
    };
    let cursor = fixture_file(temp.path(), cursor_name, b"desktop-cursor");
    let cursor_contract = ProviderDiscoveryContract::for_kind(ProviderKind::Cursor);
    assert!(cursor_contract
        .validate(ProviderDiscoveryCandidateInput::configured_override(cursor))
        .is_err());
}

#[cfg(windows)]
#[test]
fn discovery_accepts_only_the_controlled_runnable_windows_shim_contract() {
    let temp = tempdir().unwrap();
    let fixture_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/providers/registry");
    let target = temp.path().join("claude.exe");
    let shim = temp.path().join("claude.cmd");
    fs::copy(fixture_root.join("identity_claude.exe"), &target).unwrap();
    fs::copy(fixture_root.join("identity_claude.cmd"), &shim).unwrap();
    let contract = ProviderDiscoveryContract::for_kind(ProviderKind::ClaudeCode);
    let candidate = contract
        .validate(ProviderDiscoveryCandidateInput::windows_shim(
            shim.clone(),
            target.clone(),
            ProviderDiscoveryOrigin::PathEntry {
                index: 0,
                directory: temp.path().to_path_buf(),
            },
        ))
        .unwrap();
    assert!(matches!(
        candidate.form(),
        ProviderExecutableForm::WindowsShim { .. }
    ));

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
    let path = fixture_file(temp.path(), "provider-native.exe", b"provider-a");
    let replacement_path = fixture_file(temp.path(), "provider-other.exe", b"provider-b");
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
    let path = fixture_file(temp.path(), "provider-native.exe", b"provider-a");
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
}

#[test]
fn cache_hit_auth_observation_is_fresh_and_correlated_without_mutating_stable_cache() {
    let temp = tempdir().unwrap();
    let path = fixture_file(temp.path(), "provider-native.exe", b"provider-a");
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
