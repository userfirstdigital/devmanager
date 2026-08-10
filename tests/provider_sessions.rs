use devmanager::domain::{AgentRole, AgentSessionFacts, ProviderSessionId, TaskId};
use devmanager::providers::capabilities::{
    CapabilitySupport, ProviderAuthState, ProviderCapabilities, ProviderExecutable, ProviderKind,
    ProviderVersion,
};
use devmanager::providers::session::{
    ExactResumeFailure, ProviderLaunchError, ProviderLaunchMode, ProviderProcess,
    ProviderProcessId, ProviderProcessLauncher, ProviderSessionError, ProviderSessionManager,
    ProviderSessionStartMode, ProviderViewKind, StartProviderSessionRequest,
};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct LaunchRecord {
    launches: Vec<devmanager::providers::session::ProviderRuntimeLaunchRequest>,
    stopped: Vec<ProviderProcessId>,
    next_process_id: u64,
    next_error: Option<ProviderLaunchError>,
}

#[derive(Clone, Debug)]
struct FakeLauncher {
    record: Arc<Mutex<LaunchRecord>>,
}

impl FakeLauncher {
    fn new() -> (Self, Arc<Mutex<LaunchRecord>>) {
        let record = Arc::new(Mutex::new(LaunchRecord::default()));
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
}

impl ProviderProcessLauncher for FakeLauncher {
    fn launch(
        &mut self,
        request: &devmanager::providers::session::ProviderRuntimeLaunchRequest,
    ) -> Result<ProviderProcess, ProviderLaunchError> {
        let mut record = self.record.lock().unwrap();
        record.launches.push(request.clone());
        if let Some(error) = record.next_error.take() {
            return Err(error);
        }
        record.next_process_id += 1;
        Ok(ProviderProcess::new(ProviderProcessId::new(
            record.next_process_id,
        )))
    }

    fn stop(&mut self, process: &ProviderProcess) -> Result<(), ProviderLaunchError> {
        self.record.lock().unwrap().stopped.push(process.id());
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

#[test]
fn starting_session_launches_one_process() {
    let (launcher, record) = FakeLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();

    assert_eq!(record.lock().unwrap().launches.len(), 1);
    assert_eq!(runtime.generation(), 1);
    assert_eq!(
        runtime.lifecycle(),
        devmanager::providers::session::RuntimeLifecycle::Running
    );
}

#[test]
fn open_agent_with_exact_id_defaults_to_resume() {
    let provider_session_id = ProviderSessionId::new("provider-session-1").unwrap();
    let (launcher, record) = FakeLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let runtime = manager
        .start(request(
            agent(Some(provider_session_id.clone())),
            ProviderSessionStartMode::Open,
        ))
        .unwrap();

    let record = record.lock().unwrap();
    assert_eq!(record.launches.len(), 1);
    assert_eq!(
        record.launches[0].mode(),
        &ProviderLaunchMode::ResumeExact(provider_session_id.clone())
    );
    assert_eq!(runtime.provider_session_id(), Some(provider_session_id));
}

#[test]
fn agent_without_exact_id_requires_explicit_new_conversation() {
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
fn semantic_and_terminal_views_share_generation() {
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
    assert_eq!(terminal.agent_session_id(), runtime.agent_session_id());
    assert_eq!(terminal.generation(), runtime.generation());
}

#[test]
fn closing_view_does_not_stop_session() {
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

    assert_eq!(record.lock().unwrap().stopped.len(), 0);
    assert_eq!(
        runtime.lifecycle(),
        devmanager::providers::session::RuntimeLifecycle::Running
    );
    assert!(manager.current(runtime.agent_session_id()).is_some());
}

#[test]
fn closing_task_stops_provider_tree() {
    let (launcher, record) = FakeLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();

    manager.close_task(runtime.task_id()).unwrap();

    assert_eq!(record.lock().unwrap().stopped, vec![runtime.process_id()]);
    assert_eq!(
        runtime.lifecycle(),
        devmanager::providers::session::RuntimeLifecycle::Closed
    );
}

#[test]
fn stale_hook_cannot_bind_replacement_generation() {
    let (launcher, record) = FakeLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let first = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    let mut replacement_facts = agent(None);
    replacement_facts.id = first.agent_session_id();
    replacement_facts.task_id = first.task_id();
    let replacement = manager
        .replace_generation(request(
            replacement_facts,
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    let provider_session_id = ProviderSessionId::new("provider-session-2").unwrap();

    let error = manager
        .accept_provider_session_id(first.correlation(), provider_session_id.clone())
        .unwrap_err();

    assert!(matches!(
        error,
        ProviderSessionError::StaleGeneration { .. }
    ));
    assert!(replacement.provider_session_id().is_none());
    assert_eq!(record.lock().unwrap().launches.len(), 2);
}

#[test]
fn wrong_nonce_cannot_bind_identity() {
    let (launcher, _record) = FakeLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    let correlation = runtime
        .correlation()
        .set_launch_nonce_for_test(devmanager::providers::session::LaunchNonce::new());

    let error = manager
        .accept_provider_session_id(
            correlation,
            ProviderSessionId::new("provider-session-1").unwrap(),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ProviderSessionError::WrongLaunchNonce { .. }
    ));
}

#[test]
fn different_provider_identity_cannot_rebind_runtime() {
    let (launcher, _record) = FakeLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let runtime = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    let first_id = ProviderSessionId::new("provider-session-1").unwrap();
    let second_id = ProviderSessionId::new("provider-session-2").unwrap();

    assert_eq!(
        manager
            .accept_provider_session_id(runtime.correlation(), first_id.clone())
            .unwrap(),
        devmanager::providers::session::ProviderIdentityAcceptance::Accepted
    );
    let error = manager
        .accept_provider_session_id(runtime.correlation(), second_id)
        .unwrap_err();

    assert!(matches!(
        error,
        ProviderSessionError::ProviderSessionIdRebind { .. }
    ));
    assert_eq!(runtime.provider_session_id(), Some(first_id));
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
    let record = record.lock().unwrap();
    assert_eq!(record.launches.len(), 1);
    assert!(matches!(
        record.launches[0].mode(),
        ProviderLaunchMode::ResumeExact(_)
    ));
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
    let mut replacement_facts = agent(None);
    replacement_facts.id = first.agent_session_id();
    replacement_facts.task_id = first.task_id();
    let second = manager
        .replace_generation(request(
            replacement_facts,
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
fn process_exit_allows_a_new_generation_without_reusing_old_correlation() {
    let (launcher, _record) = FakeLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher);
    let first = manager
        .start(request(
            agent(None),
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();
    manager.process_exited(first.correlation()).unwrap();
    assert_eq!(
        first.lifecycle(),
        devmanager::providers::session::RuntimeLifecycle::Exited
    );

    let mut replacement_facts = agent(None);
    replacement_facts.id = first.agent_session_id();
    replacement_facts.task_id = first.task_id();
    let second = manager
        .start(request(
            replacement_facts,
            ProviderSessionStartMode::NewConversation,
        ))
        .unwrap();

    assert_eq!(second.generation(), first.generation() + 1);
    assert!(matches!(
        manager.process_exited(first.correlation()),
        Err(ProviderSessionError::StaleGeneration { .. })
    ));
}

#[allow(dead_code)]
fn _assert_provider_executable_is_a_path(value: &ProviderExecutable) -> &Path {
    value.canonical_path()
}
