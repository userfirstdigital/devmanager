use devmanager::domain::{AgentRole, AgentSessionFacts, AgentSessionId, ProviderSessionId, TaskId};
use devmanager::process::identity::ManagedProcessId;
use devmanager::providers::capabilities::{
    CapabilitySupport, ProviderAuthState, ProviderCapabilities, ProviderExecutable, ProviderKind,
    ProviderVersion,
};
use devmanager::providers::session::{
    ExactResumeFailure, FixtureProviderProcessLauncher, FixtureProviderSessionStartIssuer,
    PersistedRuntimeLifecycle, ProviderAdapterLaunchSpec, ProviderLaunchError, ProviderLaunchMode,
    ProviderProcessId, ProviderRuntimeLaunchRequest, ProviderSessionError, ProviderSessionManager,
    ProviderSessionStartMode, ProviderSessionState, ProviderSessionStateStore, ProviderViewKind,
    RuntimeLifecycle, StartProviderSessionRequest, UnavailableProviderProcessLauncher,
    MAX_SEMANTIC_PROVIDER_VIEWS,
};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
struct SharedStateStore {
    states: Arc<Mutex<HashMap<AgentSessionId, ProviderSessionState>>>,
}

impl ProviderSessionStateStore for SharedStateStore {
    fn load(
        &self,
        agent_session_id: AgentSessionId,
    ) -> Result<Option<ProviderSessionState>, String> {
        Ok(self
            .states
            .lock()
            .expect("provider state store")
            .get(&agent_session_id)
            .cloned())
    }

    fn persist(&mut self, state: ProviderSessionState) -> Result<(), String> {
        let mut states = self.states.lock().expect("provider state store");
        if states
            .get(&state.agent_session_id())
            .is_some_and(|current| state.revision() <= current.revision())
        {
            return Err("provider state revision is not monotonic".to_string());
        }
        states.insert(state.agent_session_id(), state);
        Ok(())
    }
}

fn capabilities(kind: ProviderKind) -> ProviderCapabilities {
    ProviderCapabilities {
        kind,
        version: ProviderVersion::new("fixture-1").unwrap(),
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

fn executable() -> ProviderExecutable {
    ProviderExecutable::from_path(std::env::current_exe().unwrap()).unwrap()
}

fn agent(provider_session_id: Option<ProviderSessionId>) -> AgentSessionFacts {
    AgentSessionFacts::new(
        TaskId::new(),
        AgentRole::Primary,
        ProviderKind::ClaudeCode,
        provider_session_id,
    )
    .unwrap()
}

fn launch_spec(caps: ProviderCapabilities) -> ProviderAdapterLaunchSpec {
    let mut environment = BTreeMap::new();
    environment.insert(OsString::from("DEVMANAGER_FIXTURE"), OsString::from("1"));
    ProviderAdapterLaunchSpec::new(
        executable(),
        vec![OsString::from("--fixture"), OsString::from("--one-runtime")],
        PathBuf::from("."),
        environment,
        caps,
    )
    .unwrap()
}

fn request(
    facts: AgentSessionFacts,
    mode: ProviderSessionStartMode,
) -> StartProviderSessionRequest {
    StartProviderSessionRequest::with_launch_spec(
        facts,
        launch_spec(capabilities(ProviderKind::ClaudeCode)),
        mode,
    )
}

fn request_with_caps(
    facts: AgentSessionFacts,
    mode: ProviderSessionStartMode,
    caps: ProviderCapabilities,
) -> StartProviderSessionRequest {
    StartProviderSessionRequest::with_launch_spec(facts, launch_spec(caps), mode)
}

fn same_agent_request(
    runtime: &devmanager::providers::session::ProviderRuntime,
    mode: ProviderSessionStartMode,
) -> StartProviderSessionRequest {
    let mut facts = agent(None);
    facts.id = runtime.agent_session_id();
    facts.task_id = runtime.task_id();
    facts.runtime_generation = runtime.generation();
    request(facts, mode)
}

fn snapshot_launches(
    launcher: &FixtureProviderProcessLauncher,
) -> Vec<ProviderRuntimeLaunchRequest> {
    launcher.snapshot().launches().to_vec()
}

#[test]
fn starting_session_consumes_one_exact_adapter_spec_and_one_generation() {
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();

    let launches = snapshot_launches(&launcher);
    assert_eq!(launches.len(), 1);
    let spec = launches[0].launch_spec();
    assert_eq!(spec.provider_kind(), ProviderKind::ClaudeCode);
    assert_eq!(
        spec.arguments().cloned().collect::<Vec<_>>(),
        vec![OsString::from("--fixture"), OsString::from("--one-runtime"),]
    );
    assert_eq!(
        spec.environment()
            .get(std::ffi::OsStr::new("DEVMANAGER_FIXTURE")),
        Some(&OsString::from("1"))
    );
    assert_eq!(spec.cwd(), PathBuf::from("."));
    assert_eq!(spec.capabilities(), &capabilities(ProviderKind::ClaudeCode));
    assert_eq!(spec.generation(), 1);
    assert_eq!(spec.generation(), runtime.generation());
    assert_eq!(runtime.lifecycle(), RuntimeLifecycle::Running);
    assert_ne!(runtime.process_id().pid(), 0);
    assert_ne!(runtime.process_id().creation_time_100ns(), 0);
}

#[test]
fn legacy_start_request_fails_closed_without_an_exact_adapter_spec() {
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let error = manager
        .start(StartProviderSessionRequest::new(
            agent(None),
            executable(),
            capabilities(ProviderKind::ClaudeCode),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap_err();
    assert!(matches!(
        error,
        ProviderSessionError::AdapterLaunchSpecRequired
    ));
    assert!(snapshot_launches(&launcher).is_empty());
}

#[test]
fn production_bridge_is_typed_unavailable_and_never_synthesizes_a_root() {
    let mut manager = ProviderSessionManager::new(UnavailableProviderProcessLauncher);
    let error = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap_err();
    assert!(matches!(
        error,
        ProviderSessionError::LaunchFailed(ProviderLaunchError::BridgeUnavailable)
    ));
    assert!(manager.current(AgentSessionId::new()).is_none());
}

#[test]
fn exact_resume_requires_supported_capability_before_launch() {
    let provider_session_id = ProviderSessionId::new("resume-capability-check").unwrap();
    let launcher = FixtureProviderProcessLauncher::new();
    let mut unsupported = capabilities(ProviderKind::ClaudeCode);
    unsupported.exact_resume = CapabilitySupport::Unsupported;
    let mut manager = ProviderSessionManager::new(launcher.clone());

    let error = manager
        .start(request_with_caps(
            agent(Some(provider_session_id)),
            ProviderSessionStartMode::Open,
            unsupported,
        ))
        .unwrap_err();

    assert!(matches!(
        error,
        ProviderSessionError::ExactResumeUnavailable {
            provider: ProviderKind::ClaudeCode
        }
    ));
    assert!(snapshot_launches(&launcher).is_empty());
}

#[test]
fn unsupported_exact_resume_is_rejected_before_teardown_effects() {
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    let mut facts = agent(Some(ProviderSessionId::new("resume-before-stop").unwrap()));
    facts.id = runtime.agent_session_id();
    facts.task_id = runtime.task_id();
    facts.runtime_generation = runtime.generation();
    let mut unsupported = capabilities(ProviderKind::ClaudeCode);
    unsupported.exact_resume = CapabilitySupport::Unsupported;

    let error = manager
        .replace_generation(request_with_caps(
            facts,
            ProviderSessionStartMode::Open,
            unsupported,
        ))
        .unwrap_err();
    assert!(matches!(
        error,
        ProviderSessionError::ExactResumeUnavailable {
            provider: ProviderKind::ClaudeCode
        }
    ));
    assert_eq!(snapshot_launches(&launcher).len(), 1);
    assert!(launcher.snapshot().stopped().is_empty());
    assert_eq!(runtime.lifecycle(), RuntimeLifecycle::Running);
}

#[test]
fn open_agent_with_exact_id_defaults_to_exact_resume() {
    let provider_session_id = ProviderSessionId::new("provider-session-1").unwrap();
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let runtime = manager
        .start(request(
            agent(Some(provider_session_id.clone())),
            ProviderSessionStartMode::Open,
        ))
        .unwrap();

    assert!(matches!(
        snapshot_launches(&launcher)[0].launch_spec().mode(),
        ProviderLaunchMode::ResumeExact(ref id) if id == &provider_session_id
    ));
    assert_eq!(runtime.provider_session_id(), Some(provider_session_id));
}

#[test]
fn open_agent_without_exact_id_requires_explicit_new_conversation() {
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let error = manager
        .start(request(agent(None), ProviderSessionStartMode::Open))
        .unwrap_err();
    assert!(matches!(
        error,
        ProviderSessionError::ExplicitNewConversationRequired { .. }
    ));
}

#[test]
fn semantic_and_terminal_views_share_generation_and_process() {
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    let terminal = manager.attach_terminal_view(runtime.correlation()).unwrap();
    let semantic = manager.subscribe_semantic(runtime.correlation()).unwrap();
    assert_eq!(terminal.kind(), ProviderViewKind::RawTerminal);
    assert_eq!(semantic.kind(), ProviderViewKind::Semantic);
    assert_eq!(terminal.correlation(), semantic.correlation());
    assert_eq!(terminal.process_id(), semantic.process_id());
    assert_eq!(terminal.generation(), runtime.generation());
}

#[test]
fn views_are_raii_bounded_and_can_be_reopened_after_drop() {
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();

    let terminal = manager.attach_terminal_view(runtime.correlation()).unwrap();
    let semantic = manager.subscribe_semantic(runtime.correlation()).unwrap();
    drop(terminal);
    let _terminal_again = manager.attach_terminal_view(runtime.correlation()).unwrap();
    drop(semantic);

    let views = (0..MAX_SEMANTIC_PROVIDER_VIEWS)
        .map(|_| manager.subscribe_semantic(runtime.correlation()).unwrap())
        .collect::<Vec<_>>();
    assert!(matches!(
        manager.subscribe_semantic(runtime.correlation()),
        Err(ProviderSessionError::SemanticViewLimitReached)
    ));
    drop(views);
    assert!(manager.subscribe_semantic(runtime.correlation()).is_ok());
}

#[test]
fn closing_view_does_not_stop_or_release_the_runtime_lease() {
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    let terminal = manager.attach_terminal_view(runtime.correlation()).unwrap();
    manager.close_view(&terminal).unwrap();
    assert!(launcher.snapshot().stopped().is_empty());
    assert_eq!(launcher.snapshot().lease_drops(), 0);
    assert_eq!(runtime.lifecycle(), RuntimeLifecycle::Running);
}

#[test]
fn observed_root_exit_keeps_the_lease_until_close_settles_zero() {
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    let agent_id = runtime.agent_session_id();
    manager.process_exited(runtime.correlation()).unwrap();
    assert!(runtime.root_exit_observed());
    assert_eq!(launcher.snapshot().stopped().len(), 0);

    manager.close_agent_session(agent_id).unwrap();
    assert_eq!(launcher.snapshot().stopped().len(), 1);
    assert_eq!(runtime.lifecycle(), RuntimeLifecycle::Closed);
}

#[test]
fn replacement_waits_for_joined_zero_and_never_overlaps_generations() {
    let launcher = FixtureProviderProcessLauncher::new();
    launcher.set_joined_active_process_zero(false);
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let first = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    manager.process_exited(first.correlation()).unwrap();

    let error = manager
        .replace_generation(same_agent_request(
            &first,
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap_err();
    assert!(matches!(
        error,
        ProviderSessionError::SettlementRequired { .. }
    ));
    assert_eq!(snapshot_launches(&launcher).len(), 1);
    assert_eq!(first.lifecycle(), RuntimeLifecycle::Stopping);

    launcher.set_joined_active_process_zero(true);
    let second = manager
        .replace_generation(same_agent_request(
            &first,
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    assert_eq!(snapshot_launches(&launcher).len(), 2);
    assert_eq!(launcher.snapshot().stopped().len(), 2);
    assert_eq!(second.generation(), first.generation() + 1);
    assert_ne!(second.process_id(), first.process_id());
    assert_eq!(first.lifecycle(), RuntimeLifecycle::Replaced);
}

#[test]
fn semantic_view_drop_is_raii_bounded_and_concurrent() {
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    let views = (0..MAX_SEMANTIC_PROVIDER_VIEWS)
        .map(|_| manager.subscribe_semantic(runtime.correlation()).unwrap())
        .collect::<Vec<_>>();
    assert!(matches!(
        manager.subscribe_semantic(runtime.correlation()),
        Err(ProviderSessionError::SemanticViewLimitReached)
    ));
    let handles = views
        .into_iter()
        .map(|view| std::thread::spawn(move || drop(view)))
        .collect::<Vec<_>>();
    for handle in handles {
        handle.join().unwrap();
    }
    assert!(manager.subscribe_semantic(runtime.correlation()).is_ok());
}

#[test]
fn stop_persists_stopping_before_close_and_requires_joined_zero() {
    let store = SharedStateStore::default();
    let launcher = FixtureProviderProcessLauncher::new();
    launcher.set_joined_active_process_zero(false);
    let facts = agent(None);
    let agent_id = facts.id;
    let mut manager = ProviderSessionManager::with_state_store(launcher.clone(), store.clone());
    let runtime = manager
        .start(request(
            facts.clone(),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();

    assert!(matches!(
        manager.close_agent_session(agent_id),
        Err(ProviderSessionError::SettlementRequired { .. })
    ));
    assert_eq!(runtime.lifecycle(), RuntimeLifecycle::Stopping);
    assert_eq!(
        store.load(agent_id).unwrap().unwrap().lifecycle(),
        PersistedRuntimeLifecycle::Stopping
    );
    assert_eq!(launcher.snapshot().stopped().len(), 1);

    launcher.set_joined_active_process_zero(true);
    manager.close_agent_session(agent_id).unwrap();
    assert_eq!(runtime.lifecycle(), RuntimeLifecycle::Closed);
    assert_eq!(
        store.load(agent_id).unwrap().unwrap().lifecycle(),
        PersistedRuntimeLifecycle::Closed
    );
}

#[test]
fn failed_launch_with_a_root_settles_zero_before_clearing_starting() {
    let store = SharedStateStore::default();
    let launcher = FixtureProviderProcessLauncher::new();
    launcher.fail_next_after_start(ProviderLaunchError::SpawnFailed);
    let facts = agent(None);
    let agent_id = facts.id;
    let mut manager = ProviderSessionManager::with_state_store(launcher.clone(), store.clone());
    let error = manager
        .start(request(facts, ProviderSessionStartMode::NewConversation))
        .unwrap_err();
    assert!(matches!(
        error,
        ProviderSessionError::LaunchFailed(ProviderLaunchError::SpawnFailed)
    ));
    assert_eq!(launcher.snapshot().stopped().len(), 1);
    assert_eq!(launcher.snapshot().lease_drops(), 1);
    assert_eq!(
        store.load(agent_id).unwrap().unwrap().lifecycle(),
        PersistedRuntimeLifecycle::LaunchFailed
    );
}

#[test]
fn invalid_fence_is_retained_as_unknown_until_recovery() {
    let store = SharedStateStore::default();
    let launcher = FixtureProviderProcessLauncher::new();
    launcher.set_next_fence_valid(false);
    let facts = agent(None);
    let agent_id = facts.id;
    let mut manager = ProviderSessionManager::with_state_store(launcher.clone(), store.clone());
    assert!(matches!(
        manager.start(request(facts, ProviderSessionStartMode::NewConversation)),
        Err(ProviderSessionError::LaunchFailed(
            ProviderLaunchError::ProcessFenceMismatch
        ))
    ));
    assert_eq!(launcher.snapshot().lease_drops(), 0);
    assert_eq!(
        store.load(agent_id).unwrap().unwrap().lifecycle(),
        PersistedRuntimeLifecycle::UnknownLeaked
    );
    let task_id = store.load(agent_id).unwrap().unwrap().task_id();

    let mut reopen_facts = agent(None);
    reopen_facts.id = agent_id;
    reopen_facts.task_id = task_id;
    reopen_facts.runtime_generation = 1;
    let mut reopened = ProviderSessionManager::with_state_store(launcher.clone(), store);
    assert!(matches!(
        reopened.start(request(
            reopen_facts,
            ProviderSessionStartMode::NewConversation,
        )),
        Err(ProviderSessionError::SettlementFenceMismatch)
    ));
    assert_eq!(snapshot_launches(&launcher).len(), 1);
    assert_eq!(launcher.snapshot().lease_drops(), 0);
}

#[test]
fn failed_launch_without_zero_is_marked_unknown_and_recovered_on_reopen() {
    let store = SharedStateStore::default();
    let launcher = FixtureProviderProcessLauncher::new();
    launcher.set_joined_active_process_zero(false);
    launcher.fail_next_after_start(ProviderLaunchError::SpawnFailed);
    let facts = agent(None);
    let agent_id = facts.id;
    {
        let mut manager = ProviderSessionManager::with_state_store(launcher.clone(), store.clone());
        assert!(matches!(
            manager.start(request(
                facts.clone(),
                ProviderSessionStartMode::NewConversation
            )),
            Err(ProviderSessionError::SettlementRequired { .. })
        ));
    }
    assert_eq!(
        store.load(agent_id).unwrap().unwrap().lifecycle(),
        PersistedRuntimeLifecycle::UnknownLeaked
    );
    assert_eq!(launcher.snapshot().lease_drops(), 0);

    launcher.set_joined_active_process_zero(true);
    let mut reopened = ProviderSessionManager::with_state_store(launcher.clone(), store.clone());
    let mut reopen_facts = facts;
    reopen_facts.runtime_generation = 1;
    let runtime = reopened
        .start(request(
            reopen_facts,
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    assert_eq!(runtime.generation(), 2);
    assert_eq!(launcher.snapshot().stopped().len(), 2);
}

#[test]
fn manager_drop_transfers_lease_to_recovery_owner_without_release() {
    let store = SharedStateStore::default();
    let launcher = FixtureProviderProcessLauncher::new();
    let facts = agent(None);
    let agent_id = facts.id;
    {
        let mut manager = ProviderSessionManager::with_state_store(launcher.clone(), store.clone());
        manager
            .start(request(
                facts.clone(),
                ProviderSessionStartMode::NewConversation,
            ))
            .unwrap();
    }
    assert_eq!(launcher.snapshot().lease_drops(), 0);
    assert_eq!(
        store.load(agent_id).unwrap().unwrap().lifecycle(),
        PersistedRuntimeLifecycle::UnknownLeaked
    );

    let mut reopened = ProviderSessionManager::with_state_store(launcher.clone(), store.clone());
    let mut reopen_facts = facts;
    reopen_facts.runtime_generation = 1;
    let runtime = reopened
        .start(request(
            reopen_facts,
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    assert_eq!(runtime.generation(), 2);
    assert_eq!(launcher.snapshot().stopped().len(), 1);
}

#[test]
fn reopened_manager_can_close_unknown_generation_after_recovery() {
    let store = SharedStateStore::default();
    let launcher = FixtureProviderProcessLauncher::new();
    let facts = agent(None);
    let agent_id = facts.id;
    {
        let mut manager = ProviderSessionManager::with_state_store(launcher.clone(), store.clone());
        manager
            .start(request(facts, ProviderSessionStartMode::NewConversation))
            .unwrap();
    }

    let mut reopened = ProviderSessionManager::with_state_store(launcher.clone(), store.clone());
    reopened.close_agent_session(agent_id).unwrap();
    assert_eq!(launcher.snapshot().stopped().len(), 1);
    assert_eq!(
        store.load(agent_id).unwrap().unwrap().lifecycle(),
        PersistedRuntimeLifecycle::Closed
    );
}

#[test]
fn provider_session_start_token_is_bound_one_shot_and_persisted() {
    let store = SharedStateStore::default();
    let launcher = FixtureProviderProcessLauncher::new();
    let issuer = FixtureProviderSessionStartIssuer::default();
    let facts = agent(None);
    let agent_id = facts.id;
    let mut manager = ProviderSessionManager::with_state_store(launcher.clone(), store.clone());
    let runtime = manager
        .start(request(
            facts.clone(),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    let provider_id = ProviderSessionId::new("hook-session-1").unwrap();
    let token = issuer.issue(runtime.correlation(), provider_id.clone());
    assert!(matches!(
        manager.accept_provider_session_start(token),
        Ok(devmanager::providers::session::ProviderIdentityAcceptance::Accepted)
    ));
    assert_eq!(runtime.provider_session_id(), Some(provider_id.clone()));
    assert_eq!(
        store.load(agent_id).unwrap().unwrap().provider_session_id(),
        Some(provider_id.clone())
    );

    let replay = issuer
        .replay(runtime.correlation(), provider_id.clone())
        .unwrap();
    assert!(matches!(
        manager.accept_provider_session_start(replay),
        Err(ProviderSessionError::SessionStartReplay)
    ));
    let rebound = issuer.issue(
        runtime.correlation(),
        ProviderSessionId::new("hook-session-2").unwrap(),
    );
    assert!(matches!(
        manager.accept_provider_session_start(rebound),
        Err(ProviderSessionError::ProviderSessionIdRebind { .. })
    ));
}

#[test]
fn persisted_provider_session_id_reopens_as_exact_resume() {
    let store = SharedStateStore::default();
    let launcher = FixtureProviderProcessLauncher::new();
    let issuer = FixtureProviderSessionStartIssuer::default();
    let facts = agent(None);
    let provider_id = ProviderSessionId::new("reopen-session").unwrap();
    let first;
    {
        let mut manager = ProviderSessionManager::with_state_store(launcher.clone(), store.clone());
        first = manager
            .start(request(
                facts.clone(),
                ProviderSessionStartMode::NewConversation,
            ))
            .unwrap();
        manager
            .accept_provider_session_start(issuer.issue(first.correlation(), provider_id.clone()))
            .unwrap();
        launcher.set_joined_active_process_zero(true);
        manager
            .replace_generation(same_agent_request(&first, ProviderSessionStartMode::Open))
            .unwrap();
    }
    let launches = snapshot_launches(&launcher);
    assert!(matches!(
        launches[1].launch_spec().mode(),
        ProviderLaunchMode::ResumeExact(ref id) if id == &provider_id
    ));
}

#[test]
fn explicit_new_conversation_does_not_reuse_persisted_provider_identity() {
    let store = SharedStateStore::default();
    let launcher = FixtureProviderProcessLauncher::new();
    let issuer = FixtureProviderSessionStartIssuer::default();
    let facts = agent(None);
    let mut manager = ProviderSessionManager::with_state_store(launcher, store.clone());
    let first = manager
        .start(request(facts, ProviderSessionStartMode::NewConversation))
        .unwrap();
    manager
        .accept_provider_session_start(issuer.issue(
            first.correlation(),
            ProviderSessionId::new("old-session").unwrap(),
        ))
        .unwrap();

    let second = manager
        .replace_generation(same_agent_request(
            &first,
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    assert!(second.provider_session_id().is_none());
    assert!(store
        .load(second.agent_session_id())
        .unwrap()
        .unwrap()
        .provider_session_id()
        .is_none());
}

#[test]
fn replacement_invalidates_old_views_and_uses_one_process_per_generation() {
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let first = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    let terminal = manager.attach_terminal_view(first.correlation()).unwrap();
    let second = manager
        .replace_generation(same_agent_request(
            &first,
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();

    assert_ne!(first.generation(), second.generation());
    assert_ne!(first.launch_nonce(), second.launch_nonce());
    assert_ne!(first.process_id(), second.process_id());
    assert!(matches!(
        manager.close_view(&terminal),
        Err(ProviderSessionError::StaleGeneration { .. })
    ));
    assert_eq!(snapshot_launches(&launcher).len(), 2);
    assert_eq!(launcher.snapshot().stopped(), &[first.process_id()]);
}

#[test]
fn exact_resume_failure_is_visible_without_fresh_fallback() {
    let provider_session_id = ProviderSessionId::new("missing-provider-session").unwrap();
    let launcher = FixtureProviderProcessLauncher::new();
    launcher.fail_next(ProviderLaunchError::ExactResumeFailed(
        ExactResumeFailure::NotFound,
    ));
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let error = manager
        .start(request(
            agent(Some(provider_session_id.clone())),
            ProviderSessionStartMode::Open,
        ))
        .unwrap_err();
    assert!(matches!(
        error,
        ProviderSessionError::ExactResumeFailed {
            provider_session_id: id,
            failure: ExactResumeFailure::NotFound,
        } if id == provider_session_id
    ));
    assert!(matches!(
        snapshot_launches(&launcher)[0].launch_spec().mode(),
        ProviderLaunchMode::ResumeExact(_)
    ));
    assert_eq!(snapshot_launches(&launcher).len(), 1);
}

#[test]
fn raw_provider_session_ids_are_not_identity_authority() {
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    assert!(matches!(
        manager.accept_provider_session_id(
            runtime.correlation(),
            ProviderSessionId::new("raw-id").unwrap(),
        ),
        Err(ProviderSessionError::UntrustedSessionStart)
    ));
}

#[test]
fn stale_agent_generation_facts_are_rejected_before_launch() {
    let store = SharedStateStore::default();
    let launcher = FixtureProviderProcessLauncher::new();
    let facts = agent(None);
    let mut manager = ProviderSessionManager::with_state_store(launcher.clone(), store.clone());
    let runtime = manager
        .start(request(
            facts.clone(),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    manager
        .close_agent_session(runtime.agent_session_id())
        .unwrap();

    let error = ProviderSessionManager::with_state_store(launcher.clone(), store)
        .start(request(facts, ProviderSessionStartMode::NewConversation))
        .unwrap_err();
    assert!(matches!(error, ProviderSessionError::SessionClosed(_)));
    assert_eq!(snapshot_launches(&launcher).len(), 1);
}

#[test]
fn managed_process_identity_rejects_zero_pid_and_creation_time() {
    assert!(ManagedProcessId::new(0, 1).is_err());
    assert!(ManagedProcessId::new(1, 0).is_err());
}

#[allow(dead_code)]
fn _assert_process_id_is_opaque(value: &ProviderProcessId) -> u32 {
    value.pid()
}

#[allow(dead_code)]
fn _assert_path_is_bounded(value: &PathBuf) -> &PathBuf {
    value
}
