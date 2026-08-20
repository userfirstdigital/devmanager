use super::*;
use crate::domain::{
    AgentRole, AgentSessionFacts, AgentSessionId, AgentSessionLifecycle, ProviderKind,
    ProviderSessionId, TaskId,
};
use crate::providers::adapter::{
    AdapterDeliveryPermit, AdapterIngressUnavailable, JournalNormalizeError, LaunchProviderRequest,
    NormalizedAdapterDelivery, ProviderAdapter, ProviderArgument, ProviderError,
    ProviderLaunchSpec, ProviderRuntime, QuotaObservation, StopStrategy,
};
use crate::providers::capabilities::{
    CapabilitySupport, ProviderAuthState, ProviderCapabilities, ProviderCapability,
    ProviderDiscoveryCandidateInput, ProviderDiscoveryContract, ProviderDiscoveryOrigin,
    ProviderExecutable, ProviderExecutableHandle, ProviderVersion,
};
use crate::providers::cursor::CursorAdapter;
use crate::providers::session::{ProviderLaunchSpecError, ProviderSessionStartMode};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;

#[cfg(windows)]
use std::fs;

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

#[test]
fn stock_factory_registers_each_kind_exactly_once_in_order() {
    let registry = stock_provider_registry().expect("stock registry");
    assert_eq!(
        registered_stock_kinds(&registry),
        STOCK_PROVIDER_REGISTRATION_ORDER.to_vec()
    );
    let mut again = ProviderRegistry::new();
    register_stock_adapters(&mut again).expect("first");
    assert!(matches!(
        register_stock_adapters(&mut again),
        Err(ProviderError::DuplicateProviderKind(_))
    ));
}

#[test]
fn start_request_from_adapter_calls_build_launch_and_seals_exact_resume() {
    let observation = observation_for(ProviderKind::ClaudeCode, CapabilitySupport::Supported);
    let adapter = ScriptedAdapter::new(ProviderKind::ClaudeCode, CapabilitySupport::Supported);
    let request = start_request_from_adapter(
        agent_facts(ProviderKind::ClaudeCode, Some("provider-session-1")),
        &observation,
        &adapter,
        None,
        PathBuf::from(r"C:\workspace"),
        BTreeMap::<OsString, OsString>::new(),
        ProviderSessionStartMode::ResumeExact,
    )
    .expect("start request");
    let launched = adapter
        .last_request
        .lock()
        .expect("lock")
        .clone()
        .expect("build_launch invoked");
    assert_eq!(
        launched
            .provider_session_id()
            .map(ProviderSessionId::as_str),
        Some("provider-session-1")
    );
    assert_eq!(launched.executable(), observation.executable_handle());
    assert!(request
        .launch_spec()
        .arguments()
        .any(|argument| argument == "--resume"));
}

#[cfg(windows)]
#[test]
fn windows_shim_is_normalized_to_its_native_launch_program_before_session_persistence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("claude.exe");
    let shim = temp.path().join("claude.cmd");
    fs::copy(std::env::current_exe().expect("current exe"), &target).expect("native target");
    fs::write(&shim, "@echo off\r\ncall \"%~dp0claude.exe\" %*\r\n").expect("shim");

    let candidate = ProviderDiscoveryContract::for_kind(ProviderKind::ClaudeCode)
        .validate(ProviderDiscoveryCandidateInput::windows_shim(
            &shim,
            &target,
            ProviderDiscoveryOrigin::ConfiguredOverride,
        ))
        .expect("attested shim");
    let version = ProviderVersion::new("1.0.0-test").expect("version");
    let capabilities = ProviderCapabilities {
        kind: ProviderKind::ClaudeCode,
        version: version.clone(),
        auth_state: ProviderAuthState::Unknown,
        exact_resume: CapabilitySupport::Supported,
        semantic_events: CapabilitySupport::Unsupported,
        provider_session_id: CapabilitySupport::Supported,
        build_launch: CapabilitySupport::Supported,
        parse_signal: CapabilitySupport::Unsupported,
        cooperative_stop: CapabilitySupport::Unsupported,
        observe_quota: CapabilitySupport::Unsupported,
        evidence: vec![],
    };
    let observation = ProviderObservation::from_test_parts(
        ProviderKind::ClaudeCode,
        candidate.open_for_launch().expect("launch graph"),
        version,
        capabilities,
    )
    .expect("observation");
    let adapter = ScriptedAdapter::new(ProviderKind::ClaudeCode, CapabilitySupport::Supported);

    let request = start_request_from_adapter(
        agent_facts(ProviderKind::ClaudeCode, None),
        &observation,
        &adapter,
        None,
        temp.path().to_path_buf(),
        BTreeMap::new(),
        ProviderSessionStartMode::NewConversation,
    )
    .expect("start request");

    assert!(request.launch_spec().executable().is_native());
    assert_eq!(
        request.launch_spec().executable().canonical_path(),
        fs::canonicalize(target).expect("canonical target")
    );
    assert!(request.launch_spec().runtime_dependency().is_none());
}

#[cfg(windows)]
#[test]
fn node_script_normalized_launch_keeps_runtime_dependency_identity() {
    let temp = tempfile::tempdir().expect("tempdir");
    let interpreter_path = temp.path().join("node.exe");
    let script_path = temp.path().join("codex.js");
    let wrapper_path = temp.path().join("codex.cmd");
    fs::copy(
        std::env::current_exe().expect("current exe"),
        &interpreter_path,
    )
    .expect("interpreter");
    fs::write(&script_path, "console.log('fixture');\n").expect("script");
    fs::write(&wrapper_path, "@echo off\r\nnode \"%~dp0codex.js\" %*\r\n").expect("wrapper");

    let interpreter = ProviderExecutable::from_path(&interpreter_path).expect("native interpreter");
    let script = ProviderExecutable::inspect_non_native_blocking(&script_path).expect("script");
    let wrapper = ProviderExecutable::inspect_non_native_blocking(&wrapper_path).expect("wrapper");
    let handle = wrapper
        .open_for_launch_form(
            &crate::providers::capabilities::ProviderExecutableForm::WindowsNodeScript {
                interpreter: Box::new(interpreter.clone()),
                script: Box::new(script.clone()),
            },
        )
        .expect("script launch graph");
    let version = ProviderVersion::new("1.0.0-test").expect("version");
    let capabilities = ProviderCapabilities {
        kind: ProviderKind::Codex,
        version: version.clone(),
        auth_state: ProviderAuthState::Unknown,
        exact_resume: CapabilitySupport::Supported,
        semantic_events: CapabilitySupport::Unsupported,
        provider_session_id: CapabilitySupport::Supported,
        build_launch: CapabilitySupport::Supported,
        parse_signal: CapabilitySupport::Unsupported,
        cooperative_stop: CapabilitySupport::Unsupported,
        observe_quota: CapabilitySupport::Unsupported,
        evidence: vec![],
    };
    let observation =
        ProviderObservation::from_test_parts(ProviderKind::Codex, handle, version, capabilities)
            .expect("observation");
    let adapter = ScriptedAdapter::new(ProviderKind::Codex, CapabilitySupport::Supported);

    let request = start_request_from_adapter(
        agent_facts(ProviderKind::Codex, None),
        &observation,
        &adapter,
        None,
        temp.path().to_path_buf(),
        BTreeMap::new(),
        ProviderSessionStartMode::NewConversation,
    )
    .expect("start request");

    assert!(request.launch_spec().executable().is_native());
    assert_eq!(
        request.launch_spec().executable().canonical_path(),
        interpreter.canonical_path()
    );
    let dependency = request
        .launch_spec()
        .runtime_dependency()
        .expect("script dependency");
    assert_eq!(dependency.canonical_path(), script.canonical_path());
    assert_eq!(dependency.sha256(), script.sha256());
    assert!(!dependency.is_native());
}

#[test]
fn exact_resume_fails_visibly_when_adapter_rejects_unsupported_resume() {
    let observation = observation_for(ProviderKind::Cursor, CapabilitySupport::Unsupported);
    let adapter = ScriptedAdapter::new(ProviderKind::Cursor, CapabilitySupport::Unsupported);
    let err = start_request_from_adapter(
        agent_facts(ProviderKind::Cursor, Some("cursor-session")),
        &observation,
        &adapter,
        None,
        PathBuf::from(r"C:\workspace"),
        BTreeMap::new(),
        ProviderSessionStartMode::ResumeExact,
    )
    .expect_err("exact resume unsupported");
    assert_eq!(
        err,
        ProviderBridgeError::Adapter(ProviderError::UnsupportedCapability(
            ProviderCapability::ExactResume
        ))
    );
}

#[test]
fn exact_resume_fails_when_session_id_missing_before_build_launch() {
    let observation = observation_for(ProviderKind::ClaudeCode, CapabilitySupport::Supported);
    let adapter = ScriptedAdapter::new(ProviderKind::ClaudeCode, CapabilitySupport::Supported);
    let err = start_request_from_adapter(
        agent_facts(ProviderKind::ClaudeCode, None),
        &observation,
        &adapter,
        None,
        PathBuf::from(r"C:\workspace"),
        BTreeMap::new(),
        ProviderSessionStartMode::ResumeExact,
    )
    .expect_err("resume requires id");
    assert_eq!(
        err,
        ProviderBridgeError::LaunchSpec(ProviderLaunchSpecError::ResumeIntentMismatch)
    );
    assert!(adapter.last_request.lock().expect("lock").is_none());
}

#[test]
fn cursor_normalize_and_free_ingress_stay_unavailable() {
    let adapter = CursorAdapter::new();
    assert!(matches!(
        crate::providers::journal::stock_adapter_ingress(),
        Err(_)
    ));
    assert!(!crate::providers::journal::stock_adapter_ingress_available());
    assert!(matches!(
        adapter.normalize_delivery(
            &AdapterDeliveryPermit::issue_for_test(
                ProviderKind::ClaudeCode,
                TaskId::parse("018f60b0-9c1a-7001-8000-00000000000b").unwrap(),
                AgentSessionId::parse("018f60b0-9c1a-7001-8000-000000000021").unwrap(),
                crate::domain::ResourceId::parse("018f60b0-9c1a-7001-8000-000000000057").unwrap(),
                1,
                1,
                [9u8; 32],
                "wrong-provider",
                1_725_000_000_000,
                1_725_000_060_000,
            )
            .unwrap(),
            br"{}"
        ),
        Err(JournalNormalizeError::Unavailable(_))
    ));
}

#[tokio::test]
async fn cursor_observe_quota_is_unsupported_capability() {
    let handle = current_executable().open_for_launch().unwrap();
    let adapter = CursorAdapter::new();
    assert_eq!(
        adapter.observe_quota(&handle).await.unwrap_err(),
        ProviderError::UnsupportedCapability(ProviderCapability::ObserveQuota)
    );
}
