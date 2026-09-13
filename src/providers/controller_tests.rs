use super::*;
use crate::domain::{
    AgentRole, AgentSessionFacts, AgentSessionId, AgentSessionLifecycle, ProviderKind,
    ProviderSessionId, TaskId,
};
use crate::providers::adapter::{
    AdapterDeliveryPermit, AdapterIngressUnavailable, JournalNormalizeError, LaunchProviderRequest,
    NormalizedAdapterDelivery, ProviderAdapter, ProviderArgument, ProviderError, ProviderInput,
    ProviderLaunchSpec, ProviderRuntime as AdapterRuntime, QuotaObservation, StopStrategy,
};
use crate::providers::capabilities::{
    CapabilitySupport, EvidenceConfidence, ProviderAuthEvidenceRegistry, ProviderAuthProbeResult,
    ProviderAuthState, ProviderCapabilities, ProviderCapability, ProviderExecutable,
    ProviderExecutableHandle, ProviderVersion,
};
use crate::providers::claude::ClaudeCodeAdapter;
use crate::providers::cursor::CursorAdapter;
use crate::providers::registry::ProviderObservation;
use crate::providers::session::{
    ExactResumeFailure, FixtureProviderProcessLauncher, InMemoryProviderSessionStateStore,
    ProviderLaunchError, ProviderProcessLauncher, ProviderRuntime, ProviderSessionError,
    ProviderSessionManager, ProviderSessionStartMode,
};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

fn current_executable() -> ProviderExecutable {
    ProviderExecutable::from_path(std::env::current_exe().expect("current exe")).expect("identity")
}

fn agent_facts(kind: ProviderKind, session: Option<&str>) -> AgentSessionFacts {
    AgentSessionFacts {
        id: AgentSessionId::parse("018f60b0-9c1a-7001-8000-000000000021").expect("agent"),
        task_id: TaskId::parse("018f60b0-9c1a-7001-8000-00000000000b").expect("task"),
        role: AgentRole::Primary,
        provider_kind: kind,
        provider_session_id: session.map(|value| ProviderSessionId::new(value).expect("session")),
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 1,
        revision: 0,
    }
}

fn capabilities(
    kind: ProviderKind,
    exact_resume: CapabilitySupport,
    auth_state: ProviderAuthState,
) -> ProviderCapabilities {
    ProviderCapabilities {
        kind,
        version: ProviderVersion::new("1.0.0-test").expect("version"),
        auth_state,
        exact_resume,
        semantic_events: CapabilitySupport::Unsupported,
        provider_session_id: exact_resume,
        build_launch: CapabilitySupport::Supported,
        parse_signal: CapabilitySupport::Unsupported,
        cooperative_stop: CapabilitySupport::Unsupported,
        observe_quota: CapabilitySupport::Unsupported,
        evidence: vec![],
    }
}

fn observation_without_receipt(
    kind: ProviderKind,
    exact_resume: CapabilitySupport,
) -> ProviderObservation {
    let executable = current_executable();
    let version = ProviderVersion::new("1.0.0-test").expect("version");
    let handle = executable.open_for_launch().expect("handle");
    ProviderObservation::from_test_parts(
        kind,
        handle,
        version,
        capabilities(kind, exact_resume, ProviderAuthState::Unknown),
    )
    .expect("observation")
}

fn observation_with_subscription(
    kind: ProviderKind,
    exact_resume: CapabilitySupport,
) -> ProviderObservation {
    let executable = current_executable();
    let version = ProviderVersion::new("1.0.0-test").expect("version");
    let handle = executable.open_for_launch().expect("handle");
    let stable = capabilities(kind, exact_resume, ProviderAuthState::Unknown);
    let mut registry = ProviderAuthEvidenceRegistry::new();
    let invocation = registry
        .begin_with_version(kind, executable, version.clone(), Duration::from_secs(30))
        .expect("auth invocation");
    let probe = crate::providers::capabilities::ProviderAuthProbeObservation::from_bounded_probe(
        &invocation,
        ProviderAuthProbeResult::AuthenticatedSubscription,
        EvidenceConfidence::High,
    )
    .expect("auth observation");
    let receipt = registry
        .accept_observation(invocation, probe)
        .expect("subscription receipt");
    let capabilities = stable.with_auth_receipt(&receipt).expect("merge receipt");
    ProviderObservation::from_test_parts(kind, handle, version, capabilities).expect("observation")
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
        _context: &crate::providers::adapter::ProviderProbeContext,
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

    fn cooperative_stop(&self, _session: &AdapterRuntime) -> StopStrategy {
        StopStrategy::Unsupported
    }

    async fn observe_quota(
        &self,
        _executable: &ProviderExecutableHandle,
    ) -> Result<Option<QuotaObservation>, ProviderError> {
        Ok(None)
    }
}

fn start_controller<L: ProviderProcessLauncher>(
    manager: &mut ProviderSessionManager<L, InMemoryProviderSessionStateStore>,
    agent: AgentSessionFacts,
    observation: &ProviderObservation,
    adapter: &dyn ProviderAdapter,
    mode: ProviderSessionStartMode,
) -> Result<ProviderRuntime, StockProviderSessionError> {
    StockProviderSessionController::new().start(
        manager,
        agent,
        observation,
        adapter,
        None::<ProviderInput>,
        PathBuf::from(r"C:\workspace"),
        BTreeMap::<OsString, OsString>::new(),
        mode,
    )
}

#[test]
fn unknown_auth_is_fail_closed_and_does_not_launch() {
    for kind in [
        ProviderKind::ClaudeCode,
        ProviderKind::Codex,
        ProviderKind::Cursor,
    ] {
        let launcher = FixtureProviderProcessLauncher::new();
        let mut manager = ProviderSessionManager::new(launcher.clone());
        let observation = observation_without_receipt(kind, CapabilitySupport::Supported);
        let adapter = ScriptedAdapter::new(kind, CapabilitySupport::Supported);
        let error = start_controller(
            &mut manager,
            agent_facts(kind, None),
            &observation,
            &adapter,
            ProviderSessionStartMode::NewConversation,
        )
        .expect_err("unknown auth must fail closed");
        assert_eq!(
            error,
            StockProviderSessionError::Session(ProviderSessionError::LaunchFailed(
                ProviderLaunchError::AuthenticationRequired
            )),
            "{kind:?}"
        );
        assert!(
            launcher.snapshot().launches().is_empty(),
            "{kind:?} must not reach the process launcher"
        );
        assert!(adapter.last_request.lock().expect("lock").is_none());
    }
}

#[test]
fn exact_resume_without_subscription_stays_typed_and_does_not_fallback() {
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let observation =
        observation_without_receipt(ProviderKind::ClaudeCode, CapabilitySupport::Supported);
    let adapter = ScriptedAdapter::new(ProviderKind::ClaudeCode, CapabilitySupport::Supported);
    let error = start_controller(
        &mut manager,
        agent_facts(ProviderKind::ClaudeCode, Some("provider-session-1")),
        &observation,
        &adapter,
        ProviderSessionStartMode::ResumeExact,
    )
    .expect_err("exact resume auth failure must stay visible");
    assert_eq!(
        error,
        StockProviderSessionError::Session(ProviderSessionError::ExactResumeFailed {
            provider_session_id: ProviderSessionId::new("provider-session-1").expect("session"),
            failure: ExactResumeFailure::AuthRequired,
        })
    );
    assert!(launcher.snapshot().launches().is_empty());
    assert!(adapter.last_request.lock().expect("lock").is_none());
}

#[tokio::test]
async fn cursor_unknown_auth_and_exact_resume_are_rejected() {
    const PINNED_VERSION: &[u8] =
        include_bytes!("../../tests/fixtures/providers/cursor/version.txt");
    const PINNED_HELP: &[u8] = include_bytes!("../../tests/fixtures/providers/cursor/help.txt");
    let adapter = CursorAdapter::from_pinned_probes(PINNED_VERSION, PINNED_HELP, 1_700_000_000_400);
    let handle = current_executable().open_for_launch().expect("handle");
    let capabilities = adapter
        .probe(
            &handle,
            &crate::providers::adapter::ProviderProbeContext::default(),
        )
        .await
        .expect("pinned cursor probe");
    assert_eq!(capabilities.auth_state, ProviderAuthState::Unknown);
    assert_eq!(capabilities.exact_resume, CapabilitySupport::Unsupported);
    let observation = ProviderObservation::from_test_parts(
        ProviderKind::Cursor,
        handle,
        capabilities.version.clone(),
        capabilities,
    )
    .expect("cursor observation");

    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let fresh = start_controller(
        &mut manager,
        agent_facts(ProviderKind::Cursor, None),
        &observation,
        &adapter,
        ProviderSessionStartMode::NewConversation,
    )
    .expect_err("cursor without subscription must not mint a runtime");
    assert_eq!(
        fresh,
        StockProviderSessionError::Session(ProviderSessionError::LaunchFailed(
            ProviderLaunchError::AuthenticationRequired
        ))
    );

    let resume = start_controller(
        &mut manager,
        agent_facts(ProviderKind::Cursor, Some("cursor-must-not-resume")),
        &observation,
        &adapter,
        ProviderSessionStartMode::ResumeExact,
    )
    .expect_err("cursor exact resume must stay unsupported");
    assert_eq!(
        resume,
        StockProviderSessionError::Session(ProviderSessionError::ExactResumeUnavailable {
            provider: ProviderKind::Cursor
        })
    );
    assert!(launcher.snapshot().launches().is_empty());
}

#[test]
fn subscription_launch_uses_adapter_resume_flags() {
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let observation =
        observation_with_subscription(ProviderKind::ClaudeCode, CapabilitySupport::Supported);
    let adapter = ClaudeCodeAdapter::from_attested_observation(observation.clone())
        .expect("attested Claude adapter");
    let runtime = start_controller(
        &mut manager,
        agent_facts(ProviderKind::ClaudeCode, Some("claude-session-1")),
        &observation,
        &adapter,
        ProviderSessionStartMode::ResumeExact,
    )
    .expect("subscription exact resume");
    assert_eq!(runtime.provider_kind(), ProviderKind::ClaudeCode);
    let snapshot = launcher.snapshot();
    assert_eq!(snapshot.launches().len(), 1);
    let arguments: Vec<_> = snapshot.launches()[0]
        .launch_spec()
        .arguments()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        arguments,
        vec![
            "--resume".to_string(),
            "claude-session-1".to_string(),
            "--permission-mode".to_string(),
            "bypassPermissions".to_string(),
        ]
    );
}

#[test]
fn adapter_exact_resume_failure_does_not_open_fresh() {
    let launcher = FixtureProviderProcessLauncher::new();
    let mut manager = ProviderSessionManager::new(launcher.clone());
    let observation =
        observation_with_subscription(ProviderKind::ClaudeCode, CapabilitySupport::Unsupported);
    let adapter = ClaudeCodeAdapter::from_attested_observation(observation.clone())
        .expect("attested Claude adapter");
    let error = start_controller(
        &mut manager,
        agent_facts(ProviderKind::ClaudeCode, Some("missing-session")),
        &observation,
        &adapter,
        ProviderSessionStartMode::ResumeExact,
    )
    .expect_err("unsupported exact resume");
    assert_eq!(
        error,
        StockProviderSessionError::Adapter(ProviderError::UnsupportedCapability(
            ProviderCapability::ExactResume
        ))
    );
    assert!(launcher.snapshot().launches().is_empty());
}
