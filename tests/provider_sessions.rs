use devmanager::domain::operation::ResourceFence;
use devmanager::domain::{AgentRole, AgentSessionFacts, AgentSessionId, ProviderSessionId, TaskId};
use devmanager::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
use devmanager::process::registry::ManagedProcessFence;
use devmanager::providers::capabilities::{
    CapabilitySupport, ProviderAuthState, ProviderCapabilities, ProviderExecutable, ProviderKind,
    ProviderVersion,
};
use devmanager::providers::session::{
    ActiveProcessZeroSettlement, ExactResumeFailure, ProviderLaunchError, ProviderLaunchMode,
    ProviderProcessId, ProviderProcessLauncher, ProviderProcessLease, ProviderRuntimeLaunchRequest,
    ProviderSessionError, ProviderSessionManager, ProviderSessionStartMode, ProviderSessionState,
    ProviderSessionStateStore, ProviderViewKind, RuntimeLifecycle, StartProviderSessionRequest,
    MAX_SEMANTIC_PROVIDER_VIEWS,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct LaunchRecord {
    launches: Vec<ProviderRuntimeLaunchRequest>,
    stopped: Vec<ProviderProcessId>,
    next_process_id: u32,
    next_error: Option<ProviderLaunchError>,
    stop_error: Option<ProviderLaunchError>,
    joined_active_process_zero: bool,
    lease_drops: usize,
}

#[derive(Debug)]
struct FakeLease {
    fence: ManagedProcessFence,
    record: Arc<Mutex<LaunchRecord>>,
}

impl Drop for FakeLease {
    fn drop(&mut self) {
        self.record.lock().unwrap().lease_drops += 1;
    }
}

impl ProviderProcessLease for FakeLease {
    fn fence(&self) -> &ManagedProcessFence {
        &self.fence
    }
}

#[derive(Debug)]
struct FakeSettlement {
    fence: ManagedProcessFence,
    joined_active_process_zero: bool,
}

impl ActiveProcessZeroSettlement for FakeSettlement {
    fn fence(&self) -> &ManagedProcessFence {
        &self.fence
    }

    fn is_joined_active_process_zero(&self) -> bool {
        self.joined_active_process_zero
    }
}

#[derive(Clone, Debug)]
struct FakeLauncher {
    record: Arc<Mutex<LaunchRecord>>,
}

impl FakeLauncher {
    fn new() -> (Self, Arc<Mutex<LaunchRecord>>) {
        let record = Arc::new(Mutex::new(LaunchRecord {
            joined_active_process_zero: true,
            ..LaunchRecord::default()
        }));
        (
            Self {
                record: Arc::clone(&record),
            },
            record,
        )
    }

    fn fail_next(&self, error: ProviderLaunchError) {
        self.record.lock().unwrap().next_error = Some(error);
    }

    fn set_joined_active_process_zero(&self, joined: bool) {
        self.record.lock().unwrap().joined_active_process_zero = joined;
    }

    fn next_fence(
        record: &mut LaunchRecord,
        request: &ProviderRuntimeLaunchRequest,
    ) -> ManagedProcessFence {
        record.next_process_id = record.next_process_id.saturating_add(1);
        let process_id = ManagedProcessId::new(
            record.next_process_id,
            u64::from(record.next_process_id) + 100,
        )
        .expect("fixture process identity is non-zero");
        let root = ManagedProcessIdentity::new(
            process_id,
            request.launch_spec().executable().canonical_path(),
        )
        .expect("fixture executable identity is canonicalizable");
        ManagedProcessFence::new(
            ResourceFence::new(
                request.launch_spec().resource_id(),
                request.launch_spec().generation(),
            ),
            ProcessOwner::Task(request.launch_spec().task_id()),
            root,
        )
    }
}

impl ProviderProcessLauncher for FakeLauncher {
    type Lease = FakeLease;
    type Settlement = FakeSettlement;

    fn launch(
        &mut self,
        request: &ProviderRuntimeLaunchRequest,
    ) -> Result<Self::Lease, ProviderLaunchError> {
        let mut record = self.record.lock().unwrap();
        record.launches.push(request.clone());
        if let Some(error) = record.next_error.take() {
            return Err(error);
        }
        let fence = Self::next_fence(&mut record, request);
        Ok(FakeLease {
            fence,
            record: Arc::clone(&self.record),
        })
    }

    fn stop_and_join(
        &mut self,
        lease: &mut Self::Lease,
    ) -> Result<Self::Settlement, ProviderLaunchError> {
        let mut record = self.record.lock().unwrap();
        record.stopped.push(lease.process_id());
        if let Some(error) = record.stop_error.take() {
            return Err(error);
        }
        Ok(FakeSettlement {
            fence: lease.fence.clone(),
            joined_active_process_zero: record.joined_active_process_zero,
        })
    }
}

#[derive(Clone, Debug, Default)]
struct SharedStateStore {
    states: Arc<Mutex<HashMap<AgentSessionId, ProviderSessionState>>>,
}

impl ProviderSessionStateStore for SharedStateStore {
    fn load(
        &self,
        agent_session_id: AgentSessionId,
    ) -> Result<Option<ProviderSessionState>, String> {
        Ok(self.states.lock().unwrap().get(&agent_session_id).cloned())
    }

    fn persist(&mut self, state: ProviderSessionState) -> Result<(), String> {
        let mut states = self.states.lock().unwrap();
        if states
            .get(&state.agent_session_id())
            .is_some_and(|current| state.revision() <= current.revision())
        {
            return Err("state revision is not monotonic".to_string());
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

fn request(
    facts: AgentSessionFacts,
    mode: ProviderSessionStartMode,
) -> StartProviderSessionRequest {
    StartProviderSessionRequest::new(
        facts,
        executable(),
        capabilities(ProviderKind::ClaudeCode),
        mode,
    )
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

#[test]
fn starting_session_uses_one_sealed_launch_spec_and_managed_fence() {
    let (launcher, record) = FakeLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();

    let record = record.lock().unwrap();
    assert_eq!(record.launches.len(), 1);
    let launch = &record.launches[0];
    let spec = launch.launch_spec();
    assert_eq!(launch.provider_kind(), ProviderKind::ClaudeCode);
    assert_eq!(spec.provider_kind(), ProviderKind::ClaudeCode);
    assert_eq!(spec.task_id(), runtime.task_id());
    assert_eq!(spec.resource_id(), launch.resource_id());
    assert_eq!(spec.terminal_id(), launch.terminal_id());
    assert_eq!(spec.generation(), runtime.generation());
    assert_eq!(spec.launch_nonce(), runtime.launch_nonce());
    assert_eq!(
        spec.executable().canonical_path(),
        runtime.executable().canonical_path()
    );
    assert_eq!(spec.cwd(), Path::new(&std::env::current_dir().unwrap()));
    assert!(spec.arguments().next().is_none());
    assert!(spec.environment().is_empty());
    assert_eq!(
        runtime.fence().resource(),
        ResourceFence::new(spec.resource_id(), spec.generation())
    );
    assert_eq!(runtime.fence().owner(), ProcessOwner::Task(spec.task_id()));
    assert_ne!(runtime.process_id().pid(), 0);
    assert_ne!(runtime.process_id().creation_time_100ns(), 0);
    assert_eq!(runtime.generation(), 1);
    assert_eq!(runtime.lifecycle(), RuntimeLifecycle::Running);
}

#[test]
fn open_agent_with_exact_id_defaults_to_exact_resume() {
    let provider_session_id = ProviderSessionId::new("provider-session-1").unwrap();
    let (launcher, record) = FakeLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let runtime = manager
        .start(request(
            agent(Some(provider_session_id.clone())),
            ProviderSessionStartMode::Open,
        ))
        .unwrap();

    assert_eq!(record.lock().unwrap().launches.len(), 1);
    assert_eq!(
        record.lock().unwrap().launches[0].launch_spec().mode(),
        &ProviderLaunchMode::ResumeExact(provider_session_id.clone())
    );
    assert_eq!(runtime.provider_session_id(), Some(provider_session_id));
}

#[test]
fn open_agent_without_exact_id_requires_explicit_new_conversation() {
    let (launcher, _record) = FakeLauncher::new();
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
fn views_are_non_copy_bounded_and_drop_releases_each_attachment() {
    let (launcher, _record) = FakeLauncher::new();
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
    drop(terminal);
    let _terminal_again = manager.attach_terminal_view(runtime.correlation()).unwrap();
    drop(semantic);

    let mut views = Vec::new();
    for _ in 0..MAX_SEMANTIC_PROVIDER_VIEWS {
        views.push(manager.subscribe_semantic(runtime.correlation()).unwrap());
    }
    assert!(matches!(
        manager.subscribe_semantic(runtime.correlation()),
        Err(ProviderSessionError::SemanticViewLimitReached)
    ));
    for view in views {
        drop(view);
    }
    assert!(manager.subscribe_semantic(runtime.correlation()).is_ok());
}

#[test]
fn concurrent_view_drop_releases_bounded_registry() {
    let (launcher, _record) = FakeLauncher::new();
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
fn closing_view_does_not_stop_or_release_process_lease() {
    let (launcher, record) = FakeLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    let terminal = manager.attach_terminal_view(runtime.correlation()).unwrap();

    manager.close_view(&terminal).unwrap();

    assert!(record.lock().unwrap().stopped.is_empty());
    assert_eq!(runtime.lifecycle(), RuntimeLifecycle::Running);
    assert!(manager.current(runtime.agent_session_id()).is_some());
}

#[test]
fn close_requires_joined_active_process_zero_before_closed_state() {
    let (launcher, record) = FakeLauncher::new();
    launcher.set_joined_active_process_zero(false);
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();

    let error = manager
        .close_agent_session(runtime.agent_session_id())
        .unwrap_err();
    assert!(matches!(
        error,
        ProviderSessionError::SettlementRequired { .. }
    ));
    assert_eq!(runtime.lifecycle(), RuntimeLifecycle::Stopping);
    assert_ne!(
        record.lock().unwrap().stopped.len(),
        0,
        "stop/join must be attempted before refusing the transition"
    );

    launcher.set_joined_active_process_zero(true);
    manager
        .close_agent_session(runtime.agent_session_id())
        .unwrap();
    assert_eq!(runtime.lifecycle(), RuntimeLifecycle::Closed);
}

#[test]
fn replacement_waits_for_joined_zero_and_never_overlaps_generations() {
    let (launcher, record) = FakeLauncher::new();
    launcher.set_joined_active_process_zero(false);
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let first = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    manager.process_exited(first.correlation()).unwrap();
    assert_eq!(first.lifecycle(), RuntimeLifecycle::Running);
    assert!(first.root_exit_observed());

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
    assert_eq!(record.lock().unwrap().launches.len(), 1);
    assert_eq!(first.lifecycle(), RuntimeLifecycle::Stopping);

    launcher.set_joined_active_process_zero(true);
    let second = manager
        .replace_generation(same_agent_request(
            &first,
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    assert_eq!(record.lock().unwrap().launches.len(), 2);
    assert_eq!(
        record.lock().unwrap().stopped,
        vec![first.process_id(), first.process_id()]
    );
    assert_eq!(second.generation(), first.generation() + 1);
    assert_ne!(second.process_id(), first.process_id());
    assert_eq!(first.lifecycle(), RuntimeLifecycle::Replaced);
}

#[test]
fn stale_and_raw_session_start_facts_are_not_identity_authority() {
    let (launcher, record) = FakeLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let first = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    let second = manager
        .replace_generation(same_agent_request(
            &first,
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    let provider_session_id = ProviderSessionId::new("provider-session-2").unwrap();

    assert!(matches!(
        manager.accept_provider_session_id(first.correlation(), provider_session_id.clone()),
        Err(ProviderSessionError::UntrustedSessionStart)
    ));
    assert!(matches!(
        manager.accept_provider_session_id(second.correlation(), provider_session_id),
        Err(ProviderSessionError::UntrustedSessionStart)
    ));
    assert!(second.provider_session_id().is_none());
    assert_eq!(record.lock().unwrap().launches.len(), 2);
}

#[test]
fn exact_resume_failure_is_visible_without_fresh_fallback() {
    let provider_session_id = ProviderSessionId::new("missing-provider-session").unwrap();
    let (launcher, record) = FakeLauncher::new();
    launcher.fail_next(ProviderLaunchError::ExactResumeFailed(
        ExactResumeFailure::NotFound,
    ));
    let mut manager = ProviderSessionManager::new(launcher);

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
        record.lock().unwrap().launches[0].launch_spec().mode(),
        ProviderLaunchMode::ResumeExact(_)
    ));
    assert_eq!(record.lock().unwrap().launches.len(), 1);
}

#[test]
fn replacement_invalidates_old_views_and_uses_one_process_per_generation() {
    let (launcher, record) = FakeLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
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
    assert_eq!(record.lock().unwrap().launches.len(), 2);
    assert_eq!(record.lock().unwrap().stopped, vec![first.process_id()]);
}

#[test]
fn manager_drop_retains_lease_until_an_explicit_settlement() {
    let (launcher, record) = FakeLauncher::new();
    {
        let mut manager = ProviderSessionManager::new(launcher);
        manager
            .start(request(
                agent(None),
                ProviderSessionStartMode::NewConversation,
            ))
            .unwrap();
    }
    assert_eq!(
        record.lock().unwrap().lease_drops,
        0,
        "manager Drop must not release an un-settled Job/PTY lease"
    );
}

#[test]
fn restart_rejects_persisted_live_generation_until_zero_settlement() {
    let store = SharedStateStore::default();
    let (launcher, record) = FakeLauncher::new();
    let agent = agent(None);
    let mut facts = agent.clone();
    let first_id = facts.id;
    {
        let mut manager = ProviderSessionManager::with_state_store(launcher.clone(), store.clone());
        manager
            .start(request(
                facts.clone(),
                ProviderSessionStartMode::NewConversation,
            ))
            .unwrap();
    }

    facts.runtime_generation = 1;
    let error = ProviderSessionManager::with_state_store(launcher, store)
        .start(request(facts, ProviderSessionStartMode::NewConversation))
        .unwrap_err();
    assert!(matches!(
        error,
        ProviderSessionError::SettlementRequired {
            agent_session_id,
            generation: 1,
        } if agent_session_id == first_id
    ));
    assert_eq!(record.lock().unwrap().launches.len(), 1);
}

#[test]
fn stale_agent_generation_facts_are_rejected_before_launch() {
    let store = SharedStateStore::default();
    let (launcher, record) = FakeLauncher::new();
    let facts = agent(None);
    let mut manager = ProviderSessionManager::with_state_store(launcher.clone(), store.clone());
    let runtime = manager
        .start(request(
            facts.clone(),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    launcher.set_joined_active_process_zero(true);
    manager
        .close_agent_session(runtime.agent_session_id())
        .unwrap();

    let error = ProviderSessionManager::with_state_store(launcher, store)
        .start(request(facts, ProviderSessionStartMode::NewConversation))
        .unwrap_err();
    assert!(matches!(error, ProviderSessionError::SessionClosed(_)));
    assert_eq!(record.lock().unwrap().launches.len(), 1);
}

#[test]
fn managed_process_identity_rejects_zero_pid_and_creation_time() {
    assert!(ManagedProcessId::new(0, 1).is_err());
    assert!(ManagedProcessId::new(1, 0).is_err());
}

#[allow(dead_code)]
fn _assert_provider_executable_is_a_path(value: &ProviderExecutable) -> &Path {
    value.canonical_path()
}
