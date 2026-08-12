use super::*;
use crate::domain::command::{SpecialistResult, SpecialistStatus};
use crate::domain::provider_input::ProviderInputAction;
use crate::domain::{
    AgentRole, AgentSessionFacts, AgentSessionId, AgentSessionLifecycle, ProviderKind,
    ProviderSessionId, ResourceFence, ResourceId, TaskId,
};
use crate::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
use crate::process::registry::ManagedProcessFence;
use crate::providers::adapter::{
    AdapterDeliveryPermit, AdapterIngressUnavailable, JournalNormalizeError, LaunchProviderRequest,
    NormalizedAdapterDelivery, ProviderAdapter, ProviderArgument, ProviderError, ProviderInput,
    ProviderLaunchSpec, ProviderRuntime, QuotaObservation, StopStrategy,
};
use crate::providers::capabilities::{
    CapabilitySupport, ProviderAuthState, ProviderCapabilities, ProviderCapability,
    ProviderExecutable, ProviderExecutableHandle, ProviderVersion,
};
use crate::providers::input::{
    sequence_bounded_input, sequence_provider_action, BoundProviderInputPort,
    ProviderInputDeliveryError, ProviderInputDeliveryIdentity, ACTION_PROVIDER_SEND_NOW,
};
use crate::providers::journal::JournalEvent;
use crate::providers::orchestrator::{
    ensure_single_primary, specialist_cancel_hold, specialist_native_child_hold,
    specialist_structured_result_hold,
};
use crate::providers::session::{
    FixtureProviderProcessLauncher, InMemoryProviderSessionStateStore, ProviderSessionStartMode,
    RuntimeLifecycle,
};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;

fn current_executable() -> ProviderExecutable {
    ProviderExecutable::from_path(std::env::current_exe().expect("current exe")).expect("identity")
}

fn observation_for(kind: ProviderKind, exact_resume: CapabilitySupport) -> ProviderObservation {
    let executable = current_executable();
    let version = ProviderVersion::new("1.0.0-test").expect("version");
    let capabilities = ProviderCapabilities {
        kind,
        version: version.clone(),
        auth_state: ProviderAuthState::Unknown,
        exact_resume,
        semantic_events: CapabilitySupport::Unsupported,
        provider_session_id: exact_resume,
        build_launch: CapabilitySupport::Supported,
        parse_signal: CapabilitySupport::Unsupported,
        cooperative_stop: CapabilitySupport::Unsupported,
        observe_quota: CapabilitySupport::Unsupported,
        evidence: vec![],
    };
    let handle = executable.open_for_launch().expect("handle");
    ProviderObservation::from_test_parts(kind, handle, version, capabilities).expect("observation")
}

fn agent_facts(
    kind: ProviderKind,
    role: AgentRole,
    session: Option<&str>,
    generation: u64,
) -> AgentSessionFacts {
    AgentSessionFacts {
        id: AgentSessionId::parse("018f60b0-9c1a-7001-8000-000000000021").expect("agent"),
        task_id: TaskId::parse("018f60b0-9c1a-7001-8000-00000000000b").expect("task"),
        role,
        provider_kind: kind,
        provider_session_id: session.map(|value| ProviderSessionId::new(value).expect("session")),
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: generation,
        revision: 0,
    }
}

struct ScriptedAdapter {
    kind: ProviderKind,
    exact_resume: CapabilitySupport,
    last_request: Mutex<Option<LaunchProviderRequest>>,
}

impl ScriptedAdapter {
    fn new(kind: ProviderKind, exact_resume: CapabilitySupport) -> Self {
        Self {
            kind,
            exact_resume,
            last_request: Mutex::new(None),
        }
    }
}

#[async_trait]
impl ProviderAdapter for ScriptedAdapter {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    async fn probe(
        &self,
        _executable: &ProviderExecutableHandle,
    ) -> Result<ProviderCapabilities, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::BuildLaunch,
        ))
    }

    fn build_launch(
        &self,
        request: LaunchProviderRequest,
    ) -> Result<ProviderLaunchSpec, ProviderError> {
        *self.last_request.lock().expect("lock") = Some(request.clone());
        if let Some(session_id) = request.provider_session_id() {
            if self.exact_resume != CapabilitySupport::Supported {
                return Err(ProviderError::UnsupportedCapability(
                    ProviderCapability::ExactResume,
                ));
            }
            return ProviderLaunchSpec::new(
                request.executable().clone(),
                vec![
                    ProviderArgument::new("--resume").unwrap(),
                    ProviderArgument::new(session_id.as_str()).unwrap(),
                ],
            )
            .map_err(|_| ProviderError::UnsupportedCapability(ProviderCapability::BuildLaunch));
        }
        ProviderLaunchSpec::new(request.executable().clone(), Vec::new())
            .map_err(|_| ProviderError::UnsupportedCapability(ProviderCapability::BuildLaunch))
    }

    fn normalize_delivery(
        &self,
        _permit: &AdapterDeliveryPermit,
        _bytes: &[u8],
    ) -> Result<NormalizedAdapterDelivery, JournalNormalizeError> {
        Err(JournalNormalizeError::Unavailable(
            AdapterIngressUnavailable,
        ))
    }

    fn cooperative_stop(&self, _session: &ProviderRuntime) -> StopStrategy {
        StopStrategy::Unsupported
    }

    async fn observe_quota(
        &self,
        _executable: &ProviderExecutableHandle,
    ) -> Result<Option<QuotaObservation>, ProviderError> {
        Ok(None)
    }
}

fn host_with_scripted(adapter: ScriptedAdapter) -> ProviderHost {
    let mut registry = ProviderRegistry::new();
    registry
        .register(Arc::new(adapter) as Arc<dyn ProviderAdapter>)
        .expect("register");
    ProviderHost::from_registry(registry)
}

fn input_identity(generation: u64) -> ProviderInputDeliveryIdentity {
    ProviderInputDeliveryIdentity {
        task_id: TaskId::parse("018f60b0-9c1a-7001-8000-00000000000b").expect("task"),
        operation_id: crate::domain::OperationId::parse("018f60b0-9c1a-7001-8000-000000000031")
            .expect("op"),
        command_id: crate::domain::CommandId::parse("018f60b0-9c1a-7001-8000-000000000032")
            .expect("cmd"),
        client_id: crate::domain::ClientId::parse("018f60b0-9c1a-7001-8000-000000000033")
            .expect("client"),
        agent_session_id: AgentSessionId::parse("018f60b0-9c1a-7001-8000-000000000021")
            .expect("agent"),
        provider_kind: ProviderKind::Codex,
        provider_session_id: ProviderSessionId::new("codex-session-1").expect("session"),
        runtime_generation: generation,
        action_epoch: 4,
        turn_id: crate::domain::TurnId::parse("018f60b0-9c1a-7001-8000-000000000034")
            .expect("turn"),
        question_id: None,
        approval_id: None,
    }
}

fn specialist_fence(generation: u64) -> ManagedProcessFence {
    let identity = ManagedProcessIdentity::new(
        ManagedProcessId::new(4242, 100).expect("pid"),
        std::env::current_exe().expect("exe"),
    )
    .expect("identity");
    ManagedProcessFence::new(
        ResourceFence::new(
            ResourceId::parse("018f60b0-9c1a-7001-8000-000000000057").expect("resource"),
            generation,
        ),
        ProcessOwner::Task(TaskId::parse("018f60b0-9c1a-7001-8000-00000000000b").expect("task")),
        identity,
    )
}

#[test]
fn stock_host_registers_adapters_exactly_once() {
    let host = ProviderHost::stock().expect("stock host");
    assert_eq!(host.registered_kinds(), stock_registration_order().to_vec());
    assert!(host.is_registered(ProviderKind::ClaudeCode));
    assert!(host.is_registered(ProviderKind::Codex));
    assert!(host.is_registered(ProviderKind::Cursor));
    assert!(host.adapter(ProviderKind::ClaudeCode).is_some());
}

#[test]
fn production_ai_admission_uses_registered_adapter_and_fails_cursor_resume() {
    let host = ProviderHost::stock().expect("stock host");
    let fresh = host
        .admit_production_ai_session(ProviderKind::ClaudeCode, None)
        .expect("new conversation");
    assert_eq!(fresh.mode(), ProviderSessionStartMode::NewConversation);
    assert!(fresh.provider_session_id().is_none());

    let resume = host
        .admit_production_ai_session(ProviderKind::Codex, Some("codex-session-1"))
        .expect("exact resume");
    assert_eq!(resume.mode(), ProviderSessionStartMode::ResumeExact);
    assert_eq!(
        resume.provider_session_id().map(ProviderSessionId::as_str),
        Some("codex-session-1")
    );

    assert_eq!(
        host.admit_production_ai_session(ProviderKind::Cursor, Some("cursor-session")),
        Err(HostLaunchError::ExactResumeUnsupported(
            ProviderKind::Cursor
        ))
    );
}

#[test]
fn start_request_and_session_manager_use_registered_adapter_launch_identity() {
    let observation = observation_for(ProviderKind::ClaudeCode, CapabilitySupport::Supported);
    let host = host_with_scripted(ScriptedAdapter::new(
        ProviderKind::ClaudeCode,
        CapabilitySupport::Supported,
    ));
    let request = host
        .start_request_from_registered_adapter(
            agent_facts(
                ProviderKind::ClaudeCode,
                AgentRole::Primary,
                Some("provider-session-1"),
                1,
            ),
            &observation,
            None,
            PathBuf::from(r"C:\workspace"),
            BTreeMap::<OsString, OsString>::new(),
            ProviderSessionStartMode::ResumeExact,
        )
        .expect("start request");
    assert!(request
        .launch_spec()
        .arguments()
        .any(|argument| argument == "--resume"));

    let mut manager = ProviderHost::session_manager(
        FixtureProviderProcessLauncher::new(),
        InMemoryProviderSessionStateStore::default(),
    );
    let runtime = manager.start(request).expect("manager start");
    assert_eq!(runtime.lifecycle(), RuntimeLifecycle::Running);
    assert_eq!(runtime.provider_kind(), ProviderKind::ClaudeCode);
    assert!(runtime.generation() >= 1);
}

#[test]
fn exact_resume_failure_stays_visible_and_does_not_open_fresh() {
    let observation = observation_for(ProviderKind::Cursor, CapabilitySupport::Unsupported);
    let host = host_with_scripted(ScriptedAdapter::new(
        ProviderKind::Cursor,
        CapabilitySupport::Unsupported,
    ));
    let err = host
        .start_request_from_registered_adapter(
            agent_facts(
                ProviderKind::Cursor,
                AgentRole::Primary,
                Some("cursor-session"),
                1,
            ),
            &observation,
            None,
            PathBuf::from(r"C:\workspace"),
            BTreeMap::new(),
            ProviderSessionStartMode::ResumeExact,
        )
        .expect_err("cursor exact resume");
    assert_eq!(
        err,
        ProviderBridgeError::Adapter(ProviderError::UnsupportedCapability(
            ProviderCapability::ExactResume
        ))
    );
}

#[test]
fn bound_input_port_cannot_manufacture_delivery_and_rejects_stale_generation() {
    let live = input_identity(3);
    let mut port = BoundProviderInputPort::bind(live.clone());
    let plan = sequence_bounded_input(ACTION_PROVIDER_SEND_NOW, b"hello").expect("plan");
    assert_eq!(
        deliver_claimed_provider_input(&mut port, live.clone(), plan.clone()),
        Err(ProviderInputDeliveryError::RuntimeAuthorityAbsent)
    );

    let mut stale = live.clone();
    stale.runtime_generation = 4;
    assert_eq!(
        deliver_claimed_provider_input(&mut port, stale, plan),
        Err(ProviderInputDeliveryError::StaleGeneration)
    );
}

#[test]
fn specialist_actions_stay_held_after_fence_correlation() {
    let facts = agent_facts(
        ProviderKind::Codex,
        AgentRole::specialist("reviewer").expect("role"),
        Some("codex-specialist-1"),
        3,
    );
    let fence = specialist_fence(3);
    let authority = SpecialistProcessAuthority::from_managed_process(&facts, &fence).expect("auth");
    assert!(correlate_specialist_authority(&authority, &facts).is_ok());
    let mut launcher = FixtureProviderProcessLauncher::new();
    let mut lease = match launcher.launch(
        &crate::providers::session::ProviderRuntimeLaunchRequest::sealed(
            crate::providers::session::RuntimeCorrelation::sealed(
                facts.task_id,
                facts.id,
                facts.provider_kind,
                3,
                4,
                crate::providers::session::LaunchNonce::new(),
            ),
            crate::providers::session::ProviderLaunchSpec::sealed(
                facts.provider_kind,
                current_executable(),
                crate::providers::session::ProviderLaunchMode::NewConversation,
                Vec::new(),
                PathBuf::from(r"C:\workspace"),
                BTreeMap::new(),
                observation_for(ProviderKind::Codex, CapabilitySupport::Supported)
                    .capabilities()
                    .clone(),
                facts.task_id,
                fence.resource().resource_id,
                crate::domain::TerminalId::new(),
                3,
                crate::providers::session::LaunchNonce::new(),
            ),
        ),
    ) {
        crate::providers::session::ProviderLaunchOutcome::Started(lease) => lease,
        other => panic!("fixture launch {other:?}"),
    };
    // Fixture permit uses a different resource unless the launch spec matches.
    assert_eq!(
        observe_specialist_native_child(&authority, &facts),
        Err(specialist_native_child_hold())
    );
    let uncorrelated = JournalEvent::from_correlated_test(
        ProviderKind::ClaudeCode,
        facts.task_id,
        facts.id,
        fence.resource().resource_id,
        3,
        4,
    );
    assert_eq!(
        accept_specialist_structured_result(
            &authority,
            &facts,
            &SpecialistResult {
                role: "specialist".into(),
                status: SpecialistStatus::Completed,
                summary: "done".into(),
                evidence: Vec::new(),
                artifacts: Vec::new(),
                workspace: None,
                commit: None,
                requested_follow_up: None,
            },
            &uncorrelated,
        ),
        Err(specialist_structured_result_hold())
    );
    let journal = JournalEvent::from_correlated_test(
        facts.provider_kind,
        facts.task_id,
        facts.id,
        fence.resource().resource_id,
        3,
        4,
    );
    let lineage = accept_specialist_structured_result(
        &authority,
        &facts,
        &SpecialistResult {
            role: "specialist".into(),
            status: SpecialistStatus::Completed,
            summary: "done".into(),
            evidence: Vec::new(),
            artifacts: Vec::new(),
            workspace: None,
            commit: None,
            requested_follow_up: None,
        },
        &journal,
    )
    .expect("journal lineage");
    assert_eq!(lineage.specialist_id(), facts.id);
    assert_eq!(lineage.journal_sequence(), 1);

    let mut stale = facts.clone();
    stale.runtime_generation = 4;
    assert_eq!(
        correlate_specialist_authority(&authority, &stale),
        Err(OrchestrationHold::ProviderRuntimeAuthorityAbsent)
    );
    assert_eq!(
        cancel_specialist_with_authority(&mut launcher, &mut lease, &authority, &stale),
        Err(specialist_cancel_hold())
    );
    assert_eq!(
        SpecialistProcessAuthority::from_managed_process(&stale, &fence),
        Err(specialist_cancel_hold())
    );
}

#[cfg(windows)]
#[test]
fn specialist_lifecycle_uses_exact_fenced_process_and_journal() {
    let manager = crate::services::ProcessManager::new();
    let mut launcher = manager.provider_process_launcher();
    let facts = agent_facts(
        ProviderKind::Codex,
        AgentRole::specialist("reviewer").expect("role"),
        Some("codex-specialist-1"),
        3,
    );
    let executable = crate::providers::capabilities::ProviderExecutable::from_path(PathBuf::from(
        r"C:\Windows\System32\cmd.exe",
    ))
    .expect("cmd");
    let resource_id =
        crate::domain::ResourceId::parse("018f60b0-9c1a-7001-8000-000000000057").expect("resource");
    let launch_nonce = crate::providers::session::LaunchNonce::new();
    let request = crate::providers::session::ProviderRuntimeLaunchRequest::sealed(
        crate::providers::session::RuntimeCorrelation::sealed(
            facts.task_id,
            facts.id,
            facts.provider_kind,
            3,
            4,
            launch_nonce,
        ),
        crate::providers::session::ProviderLaunchSpec::sealed(
            facts.provider_kind,
            executable,
            crate::providers::session::ProviderLaunchMode::ResumeExact(
                facts.provider_session_id.clone().expect("session"),
            ),
            Vec::new(),
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            BTreeMap::new(),
            observation_for(ProviderKind::Codex, CapabilitySupport::Supported)
                .capabilities()
                .clone(),
            facts.task_id,
            resource_id,
            crate::domain::TerminalId::new(),
            3,
            launch_nonce,
        ),
    );
    let crate::providers::session::ProviderLaunchOutcome::Started(mut lease) =
        launcher.launch(&request)
    else {
        panic!("expected production permit");
    };
    let authority =
        SpecialistProcessAuthority::from_managed_process(&facts, lease.fence()).expect("auth");
    let identity = ProviderInputDeliveryIdentity {
        task_id: facts.task_id,
        operation_id: crate::domain::OperationId::parse("018f60b0-9c1a-7001-8000-000000000031")
            .expect("op"),
        command_id: crate::domain::CommandId::parse("018f60b0-9c1a-7001-8000-000000000032")
            .expect("cmd"),
        client_id: crate::domain::ClientId::parse("018f60b0-9c1a-7001-8000-000000000033")
            .expect("client"),
        agent_session_id: facts.id,
        provider_kind: facts.provider_kind,
        provider_session_id: facts.provider_session_id.clone().expect("session"),
        runtime_generation: 3,
        action_epoch: 4,
        turn_id: crate::domain::TurnId::parse("018f60b0-9c1a-7001-8000-000000000034")
            .expect("turn"),
        question_id: None,
        approval_id: None,
    };
    let action = ProviderInputAction::SendNow {
        text: "review".into(),
        wait: false,
    };
    let plan = sequence_provider_action(&action).expect("plan");
    let handle = launcher
        .write_handle(identity.clone(), &lease)
        .expect("handle");
    let receipt =
        write_specialist_with_authority(&handle, &authority, &facts, &identity, &action, &plan)
            .expect("specialist write");
    assert_eq!(receipt.as_bytes(), b"review");
    let journal = JournalEvent::from_correlated_test(
        facts.provider_kind,
        facts.task_id,
        facts.id,
        lease.fence().resource().resource_id,
        3,
        4,
    );
    accept_specialist_structured_result(
        &authority,
        &facts,
        &SpecialistResult {
            role: "specialist".into(),
            status: SpecialistStatus::Completed,
            summary: "done".into(),
            evidence: Vec::new(),
            artifacts: Vec::new(),
            workspace: None,
            commit: None,
            requested_follow_up: None,
        },
        &journal,
    )
    .expect("structured result");
    let lineage = cancel_specialist_with_authority(&mut launcher, &mut lease, &authority, &facts)
        .expect("cancel");
    assert_eq!(lineage.specialist_id(), facts.id);
    assert_eq!(lineage.runtime_generation(), 3);
}

#[test]
fn specialist_start_uses_registered_adapter_and_keeps_one_primary() {
    let observation = observation_for(ProviderKind::Codex, CapabilitySupport::Supported);
    let host = host_with_scripted(ScriptedAdapter::new(
        ProviderKind::Codex,
        CapabilitySupport::Supported,
    ));
    let request = admit_specialist_start(
        &host,
        true,
        agent_facts(
            ProviderKind::Codex,
            AgentRole::specialist("reviewer").expect("role"),
            Some("codex-specialist-1"),
            3,
        ),
        &observation,
        PathBuf::from(r"C:\workspace"),
        BTreeMap::new(),
        ProviderSessionStartMode::ResumeExact,
    )
    .expect("specialist start request");
    assert_eq!(
        request.agent().role,
        AgentRole::specialist("reviewer").expect("role")
    );
    let mut manager = ProviderHost::session_manager(
        ProviderHost::unavailable_process_launcher(),
        InMemoryProviderSessionStateStore::default(),
    );
    assert!(manager.start(request).is_err());
    assert_eq!(
        host.session_manager_hold(),
        OrchestrationHold::ProviderRuntimeAuthorityAbsent
    );
    assert_eq!(
        ensure_single_primary(true, true),
        Err(OrchestrationHold::DuplicatePrimaryOwnership)
    );
    assert_eq!(
        admit_specialist_start(
            &host,
            true,
            agent_facts(ProviderKind::Codex, AgentRole::Primary, None, 3),
            &observation,
            PathBuf::from(r"C:\workspace"),
            BTreeMap::new(),
            ProviderSessionStartMode::NewConversation,
        ),
        Err(OrchestrationHold::DuplicatePrimaryOwnership)
    );
}

#[test]
fn unavailable_launcher_does_not_synthesize_a_process() {
    let mut manager = ProviderHost::session_manager(
        ProviderHost::unavailable_process_launcher(),
        InMemoryProviderSessionStateStore::default(),
    );
    let observation = observation_for(ProviderKind::ClaudeCode, CapabilitySupport::Supported);
    let host = host_with_scripted(ScriptedAdapter::new(
        ProviderKind::ClaudeCode,
        CapabilitySupport::Supported,
    ));
    let request = host
        .start_request_from_registered_adapter(
            agent_facts(ProviderKind::ClaudeCode, AgentRole::Primary, None, 1),
            &observation,
            None,
            PathBuf::from(r"C:\workspace"),
            BTreeMap::new(),
            ProviderSessionStartMode::NewConversation,
        )
        .expect("request");
    assert!(manager.start(request).is_err());
}
