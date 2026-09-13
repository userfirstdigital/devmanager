//! Public provider-session contract checks.
//!
//! Lifecycle fakes intentionally live in `src/providers/session.rs` under
//! `cfg(test)`; integration tests exercise only the production fail-closed
//! bridge and the durable store surface.

use devmanager::domain::{AgentRole, AgentSessionFacts, TaskId};
use devmanager::providers::capabilities::{
    CapabilitySupport, ProviderAuthState, ProviderCapabilities, ProviderExecutable, ProviderKind,
    ProviderVersion,
};
use devmanager::providers::session::{
    ProviderSessionError, ProviderSessionManager, ProviderSessionStartMode,
    ProviderSessionStateStore, SqliteProviderSessionStateStore, StartProviderSessionRequest,
    UnavailableProviderProcessLauncher,
};

fn capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        kind: ProviderKind::ClaudeCode,
        version: ProviderVersion::new("contract-test").unwrap(),
        auth_state: ProviderAuthState::Unknown,
        exact_resume: CapabilitySupport::Supported,
        semantic_events: CapabilitySupport::Supported,
        provider_session_id: CapabilitySupport::Supported,
        build_launch: CapabilitySupport::Supported,
        parse_signal: CapabilitySupport::Supported,
        cooperative_stop: CapabilitySupport::Supported,
        observe_quota: CapabilitySupport::Unknown,
        evidence: Vec::new(),
    }
}

fn facts() -> AgentSessionFacts {
    AgentSessionFacts::new(
        TaskId::new(),
        AgentRole::Primary,
        ProviderKind::ClaudeCode,
        None,
    )
    .unwrap()
}

#[test]
fn production_bridge_is_fail_closed_without_registry_launch_proof() {
    let root = tempfile::tempdir().unwrap();
    let store = SqliteProviderSessionStateStore::open(root.path().join("provider.sqlite"))
        .expect("durable store");
    let mut manager = ProviderSessionManager::<
        UnavailableProviderProcessLauncher,
        SqliteProviderSessionStateStore,
    >::with_state_store(UnavailableProviderProcessLauncher, store);
    let executable = ProviderExecutable::from_path(std::env::current_exe().unwrap()).unwrap();
    let error = manager
        .start(StartProviderSessionRequest::new(
            facts(),
            executable,
            capabilities(),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap_err();
    assert!(matches!(
        error,
        ProviderSessionError::AdapterLaunchSpecRequired
    ));
}

#[test]
fn durable_store_reopens_without_using_process_local_state() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("provider.sqlite");
    let first = SqliteProviderSessionStateStore::open(&path).expect("first open");
    drop(first);
    let second = SqliteProviderSessionStateStore::open(&path).expect("independent reopen");
    assert!(second
        .list_open_for_task(TaskId::new())
        .expect("enumerate journal")
        .is_empty());
}
