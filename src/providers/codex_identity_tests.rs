use super::{
    require_attestation, CodexAdapter, CodexAdmission, CodexCorrelatedLaunch, CodexIdentityError,
    CodexResumeFailure, CodexResumeObservation, CodexSemanticLaunchState,
};
use crate::ai::codex_hooks::{
    CodexHookRegistry, CodexLaunchPermit, CodexRegistryEvent, CodexRelayIngestStatus,
    MAX_CODEX_HOOK_BODY_BYTES,
};
use crate::domain::{AgentSessionId, ProviderSessionId, TaskId, MAX_PROVIDER_SESSION_ID_BYTES};
use crate::process::identity::ManagedProcessId;
use crate::providers::adapter::{
    LaunchProviderRequest, ProviderAdapter, ProviderError, ProviderProbeError, ProviderProbeKind,
    ProviderProbeRequest, ProviderProbeResult, ProviderProbeRunner,
};
use crate::providers::capabilities::{
    CapabilitySupport, ProviderAuthState, ProviderCapability, ProviderExecutable, ProviderKind,
};
use crate::providers::registry::{ProviderDiscoveryConfig, ProviderRegistry};
use async_trait::async_trait;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const FIXTURE_SESSION_ID: &str = "019f-fixture-codex-session";
const VERSION: &str = include_str!("../../tests/fixtures/providers/codex/version.txt");
const HELP: &str = include_str!("../../tests/fixtures/providers/codex/help.txt");
const HELP_TERMINAL_ONLY: &str =
    include_str!("../../tests/fixtures/providers/codex/help_terminal_only.txt");
const RESUME_HELP: &str = include_str!("../../tests/fixtures/providers/codex/resume_help.txt");
const RESUME_HELP_LAST_ONLY: &str =
    include_str!("../../tests/fixtures/providers/codex/resume_help_last_only.txt");
const RESUME_HELP_PROSE: &str =
    include_str!("../../tests/fixtures/providers/codex/resume_help_prose_session_id.txt");
const LOGIN_CHATGPT: &str =
    include_str!("../../tests/fixtures/providers/codex/login_status_chatgpt_subscription.txt");
const LOGIN_API_KEY: &str =
    include_str!("../../tests/fixtures/providers/codex/login_status_api_key.txt");
const LOGIN_NOT_AUTH: &str =
    include_str!("../../tests/fixtures/providers/codex/login_status_not_authenticated.txt");
const LOGIN_LOGGED_IN: &str =
    include_str!("../../tests/fixtures/providers/codex/login_status_logged_in_only.txt");
const LOGIN_NEGATED: &str =
    include_str!("../../tests/fixtures/providers/codex/login_status_negated.txt");
const LOGIN_EXPIRED: &str =
    include_str!("../../tests/fixtures/providers/codex/login_status_expired.txt");
const LOGIN_BURIED: &str =
    include_str!("../../tests/fixtures/providers/codex/login_status_buried.txt");
const LOGIN_NO_PLAN: &str =
    include_str!("../../tests/fixtures/providers/codex/login_status_chatgpt_no_plan.txt");
const SESSION_START: &str = include_str!("../../tests/fixtures/providers/codex/session_start.json");
const SESSION_START_MISSING_ID: &str =
    include_str!("../../tests/fixtures/providers/codex/session_start_missing_id.json");
const SESSION_START_UNKNOWN: &str =
    include_str!("../../tests/fixtures/providers/codex/session_start_unknown.json");
const SESSION_START_CONTROL: &str =
    include_str!("../../tests/fixtures/providers/codex/session_start_control.json");
const UNKNOWN_EVENT: &str = include_str!("../../tests/fixtures/providers/codex/unknown_event.json");

#[derive(Clone)]
enum ProbeScript {
    Completed {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    NonZero {
        code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    TimedOut,
}

#[derive(Clone)]
struct ScriptedProbeRunner {
    version: ProbeScript,
    help: ProbeScript,
    resume_help: ProbeScript,
    login_status: ProbeScript,
    kinds: std::sync::Arc<std::sync::Mutex<Vec<ProviderProbeKind>>>,
}

struct FailAfterProbeRunner {
    inner: Arc<ScriptedProbeRunner>,
    fail_after: usize,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl ProviderProbeRunner for FailAfterProbeRunner {
    async fn run(
        &self,
        request: ProviderProbeRequest,
    ) -> Result<ProviderProbeResult, ProviderProbeError> {
        let call = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if call >= self.fail_after {
            return Err(ProviderProbeError::TimedOut);
        }
        self.inner.run(request).await
    }
}

impl ScriptedProbeRunner {
    fn ok(version: &str, help: &str, resume_help: &str, login: &str) -> Arc<Self> {
        Arc::new(Self {
            version: ProbeScript::Completed {
                stdout: version.as_bytes().to_vec(),
                stderr: Vec::new(),
            },
            help: ProbeScript::Completed {
                stdout: help.as_bytes().to_vec(),
                stderr: Vec::new(),
            },
            resume_help: ProbeScript::Completed {
                stdout: resume_help.as_bytes().to_vec(),
                stderr: Vec::new(),
            },
            login_status: ProbeScript::Completed {
                stdout: login.as_bytes().to_vec(),
                stderr: Vec::new(),
            },
            kinds: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        })
    }
}

#[async_trait]
impl ProviderProbeRunner for ScriptedProbeRunner {
    async fn run(
        &self,
        request: ProviderProbeRequest,
    ) -> Result<ProviderProbeResult, ProviderProbeError> {
        self.kinds
            .lock()
            .expect("scripted kind lock")
            .push(request.kind());
        let script = match request.kind() {
            ProviderProbeKind::Version => &self.version,
            ProviderProbeKind::Help => &self.help,
            ProviderProbeKind::ResumeHelp => &self.resume_help,
            ProviderProbeKind::LoginStatus => &self.login_status,
            ProviderProbeKind::AuthStatus => {
                return Err(ProviderProbeError::InvalidRequest(
                    crate::providers::adapter::ProviderProbeRequestError::EmptyExecutable,
                ))
            }
        };
        match script {
            ProbeScript::TimedOut => Err(ProviderProbeError::TimedOut),
            ProbeScript::Completed { stdout, stderr } => ProviderProbeResult::from_bounded_output(
                &request,
                Some(0),
                stdout.clone(),
                stderr.clone(),
            ),
            ProbeScript::NonZero {
                code,
                stdout,
                stderr,
            } => ProviderProbeResult::from_bounded_output(
                &request,
                Some(*code),
                stdout.clone(),
                stderr.clone(),
            ),
        }
    }
}

fn fixture_executable() -> ProviderExecutable {
    ProviderExecutable::new(PathBuf::from("/fixture/codex"), [4; 32]).unwrap()
}

fn other_executable() -> ProviderExecutable {
    ProviderExecutable::new(PathBuf::from("/fixture/codex-other"), [5; 32]).unwrap()
}

fn replaced_hash_executable() -> ProviderExecutable {
    ProviderExecutable::new(PathBuf::from("/fixture/codex"), [9; 32]).unwrap()
}

fn session_id() -> ProviderSessionId {
    ProviderSessionId::new(FIXTURE_SESSION_ID).unwrap()
}

fn loopback() -> SocketAddr {
    "127.0.0.1:5555".parse().unwrap()
}

fn process_root(tail: u8) -> ManagedProcessId {
    ManagedProcessId::new(1000 + u32::from(tail), 1_700_000_000_000 + u64::from(tail)).unwrap()
}

fn issue_permit(
    registry: &Arc<CodexHookRegistry>,
    task: TaskId,
    agent: AgentSessionId,
    root: ManagedProcessId,
) -> CodexLaunchPermit {
    CodexHookRegistry::issue_launch_permit(Arc::clone(registry), task, agent, root)
        .expect("registry launch permit")
}

async fn probed(help: &str, resume_help: &str, login: &str) -> CodexAdapter {
    let adapter = CodexAdapter::new(ScriptedProbeRunner::ok(VERSION, help, resume_help, login));
    adapter
        .probe_attested(&fixture_executable())
        .await
        .expect("fixture probe");
    adapter
}

fn resume_launch(
    adapter: &CodexAdapter,
    identity: ProviderExecutable,
    tail: u8,
) -> CodexCorrelatedLaunch {
    let registry = Arc::new(CodexHookRegistry::default());
    launch_with(
        adapter,
        identity,
        Some(session_id()),
        &registry,
        TaskId::new(),
        AgentSessionId::new(),
        process_root(tail),
    )
    .expect("resume launch")
}

fn launch_with(
    adapter: &CodexAdapter,
    identity: ProviderExecutable,
    session: Option<ProviderSessionId>,
    registry: &Arc<CodexHookRegistry>,
    task: TaskId,
    agent: AgentSessionId,
    root: ManagedProcessId,
) -> Result<CodexCorrelatedLaunch, ProviderError> {
    adapter.prepare_correlated_launch(
        LaunchProviderRequest::new(
            identity
                .open_for_launch()
                .expect("fixture executable handle"),
            None,
            session,
        ),
        issue_permit(registry, task, agent, root),
        "http://127.0.0.1:9/internal/codex-hook",
        Path::new("C:/fixture/devmanager.exe"),
    )
}

fn admit_from_relay(
    launch: &mut CodexCorrelatedLaunch,
    body: &[u8],
    occurred_at_epoch_ms: u64,
) -> Result<CodexAdmission, CodexIdentityError> {
    let observation = launch.relay_ingest(loopback(), body, occurred_at_epoch_ms);
    launch.admit_ingest(observation, body)
}

fn nonce_from_spec(spec: &crate::providers::adapter::ProviderLaunchSpec) -> String {
    for argument in spec.arguments() {
        if let Some(offset) = argument.find("--nonce ") {
            let hex: String = argument[offset + 8..]
                .chars()
                .take_while(|character| character.is_ascii_hexdigit())
                .collect();
            if !hex.is_empty() {
                return hex;
            }
        }
    }
    panic!("hook nonce missing from launch spec");
}

#[tokio::test]
async fn codex_probe_uses_login_status_and_resume_help_subcommands() {
    let runner = ScriptedProbeRunner::ok(VERSION, HELP, RESUME_HELP, LOGIN_CHATGPT);
    let adapter = CodexAdapter::new(Arc::clone(&runner) as Arc<dyn ProviderProbeRunner>);
    adapter.probe_attested(&fixture_executable()).await.unwrap();
    let kinds = runner.kinds.lock().expect("kinds").clone();
    assert!(kinds.contains(&ProviderProbeKind::ResumeHelp));
    assert!(kinds.contains(&ProviderProbeKind::LoginStatus));
    assert!(!kinds.contains(&ProviderProbeKind::AuthStatus));
    assert_eq!(
        ProviderProbeKind::LoginStatus.arguments(),
        ["login", "status"]
    );
    assert_eq!(
        ProviderProbeKind::ResumeHelp.arguments(),
        ["resume", "--help"]
    );
}

#[tokio::test]
async fn codex_probe_classifies_only_strict_login_status_contract() {
    let identity = fixture_executable();
    let chatgpt = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    assert_eq!(
        chatgpt.last_capabilities(&identity).unwrap().auth_state,
        ProviderAuthState::AuthenticatedSubscription
    );

    for (label, login, expected) in [
        ("api-key", LOGIN_API_KEY, ProviderAuthState::Unknown),
        (
            "not-authenticated",
            LOGIN_NOT_AUTH,
            ProviderAuthState::AuthRequired,
        ),
        (
            "logged-in-only",
            LOGIN_LOGGED_IN,
            ProviderAuthState::Unknown,
        ),
        ("negated", LOGIN_NEGATED, ProviderAuthState::Unknown),
        ("expired", LOGIN_EXPIRED, ProviderAuthState::Unknown),
        ("buried", LOGIN_BURIED, ProviderAuthState::Unknown),
        ("no-plan", LOGIN_NO_PLAN, ProviderAuthState::Unknown),
        (
            "invented-plan-plus",
            "Logged in using ChatGPT\nplan: plus\n",
            ProviderAuthState::Unknown,
        ),
    ] {
        let adapter = probed(HELP, RESUME_HELP, login).await;
        assert_eq!(
            adapter.last_capabilities(&identity).unwrap().auth_state,
            expected,
            "{label}"
        );
    }
}

#[tokio::test]
async fn codex_truncated_login_status_cannot_authenticate() {
    let mut padded = LOGIN_CHATGPT.to_string();
    padded.push_str(&"x".repeat(5000));
    let adapter = CodexAdapter::new(ScriptedProbeRunner::ok(VERSION, HELP, RESUME_HELP, &padded));
    adapter.probe_attested(&fixture_executable()).await.unwrap();
    assert_eq!(
        adapter
            .last_capabilities(&fixture_executable())
            .unwrap()
            .auth_state,
        ProviderAuthState::Unknown
    );
}

#[tokio::test]
async fn codex_probe_timeout_nonzero_or_stderr_cannot_mint_capabilities() {
    let identity = fixture_executable();
    let mut timeout = ScriptedProbeRunner::ok(VERSION, HELP, RESUME_HELP, LOGIN_CHATGPT);
    {
        let runner = Arc::make_mut(&mut timeout);
        runner.login_status = ProbeScript::TimedOut;
    }
    let adapter = CodexAdapter::new(timeout);
    assert!(matches!(
        adapter.probe_attested(&identity).await,
        Err(ProviderError::Probe(ProviderProbeError::TimedOut))
    ));

    let mut nonzero = ScriptedProbeRunner::ok(VERSION, HELP, RESUME_HELP, LOGIN_CHATGPT);
    {
        let runner = Arc::make_mut(&mut nonzero);
        runner.version = ProbeScript::NonZero {
            code: 2,
            stdout: VERSION.as_bytes().to_vec(),
            stderr: Vec::new(),
        };
    }
    let adapter = CodexAdapter::new(nonzero);
    assert!(adapter.probe_attested(&identity).await.is_err());

    let mut stderr = ScriptedProbeRunner::ok(VERSION, HELP, RESUME_HELP, LOGIN_CHATGPT);
    {
        let runner = Arc::make_mut(&mut stderr);
        runner.login_status = ProbeScript::Completed {
            stdout: LOGIN_CHATGPT.as_bytes().to_vec(),
            stderr: b"warning".to_vec(),
        };
    }
    let adapter = CodexAdapter::new(stderr);
    assert!(adapter.probe_attested(&identity).await.is_err());
}

#[tokio::test]
async fn codex_exact_resume_requires_usage_signature_not_prose() {
    let identity = fixture_executable();
    let mut missing = ScriptedProbeRunner::ok(VERSION, HELP, RESUME_HELP, LOGIN_CHATGPT);
    {
        let runner = Arc::make_mut(&mut missing);
        runner.resume_help = ProbeScript::TimedOut;
    }
    let adapter = CodexAdapter::new(missing);
    adapter.probe_attested(&identity).await.unwrap();
    assert_eq!(
        adapter.last_capabilities(&identity).unwrap().exact_resume,
        CapabilitySupport::Unsupported
    );
    assert!(matches!(
        adapter.build_launch(LaunchProviderRequest::new(
            identity
                .open_for_launch()
                .expect("fixture executable handle"),
            None,
            Some(session_id()),
        )),
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::ExactResume
        ))
    ));

    let last_only = probed(HELP, RESUME_HELP_LAST_ONLY, LOGIN_CHATGPT).await;
    assert_eq!(
        last_only.last_capabilities(&identity).unwrap().exact_resume,
        CapabilitySupport::Unsupported
    );

    let prose = probed(HELP, RESUME_HELP_PROSE, LOGIN_CHATGPT).await;
    assert_eq!(
        prose.last_capabilities(&identity).unwrap().exact_resume,
        CapabilitySupport::Unsupported
    );

    let from_resume_help = probed(HELP_TERMINAL_ONLY, RESUME_HELP, LOGIN_CHATGPT).await;
    assert_eq!(
        from_resume_help
            .last_capabilities(&identity)
            .unwrap()
            .exact_resume,
        CapabilitySupport::Supported
    );
}

#[tokio::test]
async fn codex_trait_launch_without_correlation_is_terminal_only_dependency() {
    let identity = fixture_executable();
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    assert_eq!(
        adapter.semantic_launch_state(&identity),
        CodexSemanticLaunchState::DependencyUnavailable
    );
    let spec = adapter
        .build_launch(LaunchProviderRequest::new(
            identity
                .open_for_launch()
                .expect("fixture executable handle"),
            None,
            None,
        ))
        .unwrap();
    let arguments: Vec<&str> = spec.arguments().collect();
    assert!(arguments.iter().all(|argument| {
        *argument != "-c"
            && !argument.contains("hooks.")
            && *argument != "--dangerously-bypass-hook-trust"
            && *argument != "app-server"
            && *argument != "exec"
            && *argument != "--last"
            && *argument != "--remote"
    }));
    assert_eq!(
        adapter
            .last_capabilities(&identity)
            .unwrap()
            .semantic_events,
        CapabilitySupport::Unsupported
    );
    assert_eq!(
        adapter.last_capabilities(&identity).unwrap().parse_signal,
        CapabilitySupport::Unsupported
    );
}

#[tokio::test]
async fn codex_exact_resume_is_resume_id_and_typed_failures_do_not_fallback() {
    let identity = fixture_executable();
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let spec = adapter
        .build_launch(LaunchProviderRequest::new(
            identity
                .open_for_launch()
                .expect("fixture executable handle"),
            None,
            Some(session_id()),
        ))
        .unwrap();
    assert_eq!(
        spec.arguments().collect::<Vec<_>>(),
        ["resume", FIXTURE_SESSION_ID]
    );

    let resume_id = session_id();
    let launch = resume_launch(&adapter, identity.clone(), 9);
    assert_eq!(
        launch.settle_exact_resume(
            &identity,
            &resume_id,
            CodexResumeObservation::Failed(CodexResumeFailure::NotFound),
        ),
        Err(CodexResumeFailure::NotFound)
    );
    assert_eq!(
        launch.settle_exact_resume(
            &identity,
            &resume_id,
            CodexResumeObservation::Failed(CodexResumeFailure::Incompatible),
        ),
        Err(CodexResumeFailure::Incompatible)
    );
    assert_eq!(
        launch.settle_exact_resume(&identity, &resume_id, CodexResumeObservation::Succeeded),
        Ok(())
    );
    assert_eq!(
        launch.settle_exact_resume(
            &replaced_hash_executable(),
            &resume_id,
            CodexResumeObservation::Succeeded,
        ),
        Err(CodexResumeFailure::Incompatible)
    );
    let fresh_registry = Arc::new(CodexHookRegistry::default());
    let fresh = launch_with(
        &adapter,
        identity.clone(),
        None,
        &fresh_registry,
        TaskId::new(),
        AgentSessionId::new(),
        process_root(10),
    )
    .expect("fresh launch");
    assert_eq!(
        fresh.settle_exact_resume(&identity, &resume_id, CodexResumeObservation::Succeeded),
        Err(CodexResumeFailure::Incompatible)
    );
    let auth_required = probed(HELP, RESUME_HELP, LOGIN_NOT_AUTH).await;
    assert!(matches!(
        auth_required.build_launch(LaunchProviderRequest::new(
            identity
                .open_for_launch()
                .expect("fixture executable handle"),
            None,
            Some(session_id()),
        )),
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::ExactResume
        ))
    ));
    let auth_registry = Arc::new(CodexHookRegistry::default());
    assert!(matches!(
        launch_with(
            &auth_required,
            identity.clone(),
            Some(session_id()),
            &auth_registry,
            TaskId::new(),
            AgentSessionId::new(),
            process_root(11),
        ),
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::ExactResume
        ))
    ));
    let terminal = probed(HELP_TERMINAL_ONLY, RESUME_HELP_LAST_ONLY, LOGIN_CHATGPT).await;
    assert!(matches!(
        terminal.build_launch(LaunchProviderRequest::new(
            identity
                .open_for_launch()
                .expect("fixture executable handle"),
            None,
            Some(session_id()),
        )),
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::ExactResume
        ))
    ));
}

#[tokio::test]
async fn codex_same_path_replacement_cannot_inherit_probe_or_settlement() {
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let original = fixture_executable();
    let replaced = replaced_hash_executable();
    let other = other_executable();
    assert!(adapter.last_capabilities(&replaced).is_none());
    assert!(adapter.last_capabilities(&other).is_none());
    assert_eq!(
        adapter.semantic_launch_state(&replaced),
        CodexSemanticLaunchState::TerminalOnly
    );
    assert!(matches!(
        adapter.build_launch(LaunchProviderRequest::new(
            replaced
                .open_for_launch()
                .expect("fixture executable handle"),
            None,
            Some(session_id()),
        )),
        Err(ProviderError::ExecutableChanged { .. })
    ));
    let err = adapter
        .build_launch(LaunchProviderRequest::new(
            other.open_for_launch().expect("fixture executable handle"),
            None,
            Some(session_id()),
        ))
        .unwrap_err();
    let rendered = format!("{err:?}");
    assert!(!rendered.contains("/fixture/codex"));
    assert!(!rendered.contains(FIXTURE_SESSION_ID));
    let launch = resume_launch(&adapter, original.clone(), 12);
    let launch_debug = format!("{launch:?}");
    assert!(!launch_debug.contains(FIXTURE_SESSION_ID));
    assert!(!launch_debug.contains("/fixture/codex"));
    assert_eq!(
        launch.settle_exact_resume(&replaced, &session_id(), CodexResumeObservation::Succeeded,),
        Err(CodexResumeFailure::Incompatible)
    );

    adapter.probe_attested(&replaced).await.unwrap();
    assert!(adapter.last_capabilities(&original).is_none());
    assert_eq!(
        adapter.last_capabilities(&replaced).unwrap().auth_state,
        ProviderAuthState::AuthenticatedSubscription
    );
}

#[tokio::test]
async fn codex_correlated_launch_registers_authenticated_hooks_and_binds_first_session_start() {
    let identity = fixture_executable();
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let registry = Arc::new(CodexHookRegistry::default());
    let task = TaskId::new();
    let agent = AgentSessionId::new();
    let root = process_root(1);
    let endpoint = "http://127.0.0.1:9/internal/codex-hook";
    let mut launch = adapter
        .prepare_correlated_launch(
            LaunchProviderRequest::new(
                identity
                    .open_for_launch()
                    .expect("fixture executable handle"),
                None,
                None,
            ),
            issue_permit(&registry, task, agent, root),
            endpoint,
            Path::new("C:/fixture/devmanager.exe"),
        )
        .expect("correlated launch");
    let arguments: Vec<String> = launch.spec().arguments().map(str::to_string).collect();
    assert!(arguments.iter().any(|argument| argument == "-c"));
    assert!(arguments
        .iter()
        .any(|argument| argument.contains("hooks.SessionStart=")));
    assert!(arguments
        .iter()
        .any(|argument| argument == "--dangerously-bypass-hook-trust"));
    assert_eq!(
        adapter.semantic_launch_state(&identity),
        CodexSemanticLaunchState::Registered
    );
    assert_eq!(
        adapter
            .last_capabilities(&identity)
            .unwrap()
            .semantic_events,
        CapabilitySupport::Supported
    );
    assert_eq!(
        adapter.last_capabilities(&identity).unwrap().parse_signal,
        CapabilitySupport::Unsupported
    );
    let observation = launch.relay_ingest(loopback(), SESSION_START.as_bytes(), 1);
    assert_eq!(
        observation.status(),
        crate::ai::codex_hooks::CodexRelayIngestStatus::Accepted
    );
    match launch
        .admit_ingest(observation, SESSION_START.as_bytes())
        .expect("bind")
    {
        CodexAdmission::Bound(bound) => assert_eq!(bound.as_str(), FIXTURE_SESSION_ID),
        other => panic!("expected bind, got {other:?}"),
    }
    assert!(matches!(
        admit_from_relay(&mut launch, SESSION_START.as_bytes(), 2),
        Err(CodexIdentityError::Replay)
    ));
    assert_eq!(launch.authority().task_id(), task);
    assert_eq!(launch.authority().agent_session_id(), agent);
    assert_eq!(launch.authority().process_root(), root);
}

#[tokio::test]
async fn codex_admission_rejects_body_not_authenticated_by_relay() {
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let registry = Arc::new(CodexHookRegistry::default());
    let mut launch = launch_with(
        &adapter,
        fixture_executable(),
        None,
        &registry,
        TaskId::new(),
        AgentSessionId::new(),
        process_root(13),
    )
    .unwrap();
    let status = launch.relay_ingest(loopback(), SESSION_START.as_bytes(), 1);
    assert_eq!(
        launch.admit_ingest(status, SESSION_START_MISSING_ID.as_bytes()),
        Err(CodexIdentityError::Rejected)
    );
    assert!(launch.authority().bound_id().is_none());
}

#[tokio::test]
async fn codex_observation_cannot_cross_launch_generations() {
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let registry = Arc::new(CodexHookRegistry::default());
    let first = launch_with(
        &adapter,
        fixture_executable(),
        None,
        &registry,
        TaskId::new(),
        AgentSessionId::new(),
        process_root(15),
    )
    .unwrap();
    let mut second = launch_with(
        &adapter,
        fixture_executable(),
        None,
        &registry,
        TaskId::new(),
        AgentSessionId::new(),
        process_root(16),
    )
    .unwrap();
    let observation = first.relay_ingest(loopback(), SESSION_START.as_bytes(), 1);
    assert_eq!(
        second.admit_ingest(observation, SESSION_START.as_bytes()),
        Err(CodexIdentityError::Rejected)
    );
    assert!(second.authority().bound_id().is_none());
}

#[tokio::test]
async fn codex_admission_rejects_nested_or_array_session_start_fabrication() {
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let registry = Arc::new(CodexHookRegistry::default());
    let nested = r#"{"metadata":{"hook_event_name":"SessionStart","session_id":"nested"}}"#;
    let array = r#"[{"hook_event_name":"SessionStart","session_id":"array"}]"#;
    for (index, body) in [nested, array].into_iter().enumerate() {
        let mut launch = launch_with(
            &adapter,
            fixture_executable(),
            None,
            &registry,
            TaskId::new(),
            AgentSessionId::new(),
            process_root(20 + u8::try_from(index).unwrap()),
        )
        .unwrap();
        assert_eq!(
            admit_from_relay(&mut launch, body.as_bytes(), 1),
            Err(CodexIdentityError::Rejected)
        );
        assert!(launch.authority().bound_id().is_none());
    }
}

#[tokio::test]
async fn codex_same_identity_reprobe_failure_quarantines_previous_capabilities() {
    let inner = ScriptedProbeRunner::ok(VERSION, HELP, RESUME_HELP, LOGIN_CHATGPT);
    let runner = Arc::new(FailAfterProbeRunner {
        inner,
        fail_after: 4,
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let adapter = CodexAdapter::new(runner);
    let identity = fixture_executable();
    adapter.probe_attested(&identity).await.unwrap();
    assert!(adapter.probe_attested(&identity).await.is_err());
    assert!(matches!(
        adapter.build_launch(LaunchProviderRequest::new(
            identity
                .open_for_launch()
                .expect("fixture executable handle"),
            None,
            None,
        )),
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::BuildLaunch
        ))
    ));
}

#[tokio::test]
async fn codex_public_probe_inspection_failure_quarantines_previous_capabilities() {
    let runner = Arc::new(FailAfterProbeRunner {
        inner: ScriptedProbeRunner::ok(VERSION, HELP, RESUME_HELP, LOGIN_CHATGPT),
        fail_after: 4,
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let adapter = CodexAdapter::new(runner);
    let identity = fixture_executable();
    adapter.probe_attested(&identity).await.unwrap();
    assert!(adapter.last_capabilities(&identity).is_some());

    let handle = identity
        .open_for_launch()
        .expect("fixture executable handle");
    assert!(ProviderAdapter::probe(&adapter, &handle).await.is_err());
    assert!(adapter.last_capabilities(&identity).is_none());
    assert_eq!(
        adapter.semantic_launch_state(&identity),
        CodexSemanticLaunchState::TerminalOnly
    );
    assert!(matches!(
        adapter.build_launch(LaunchProviderRequest::new(
            identity
                .open_for_launch()
                .expect("fixture executable handle"),
            None,
            None,
        )),
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::BuildLaunch
        ))
    ));
}

#[test]
fn codex_attestation_generation_fences_stale_probe_publication() {
    let adapter = CodexAdapter::new(ScriptedProbeRunner::ok(
        VERSION,
        HELP,
        RESUME_HELP,
        LOGIN_CHATGPT,
    ));
    let identity = fixture_executable();
    let first = adapter.begin_attestation(&identity).expect("first epoch");
    let second = adapter
        .begin_attestation(&identity)
        .expect("replacement epoch");

    assert!(matches!(
        require_attestation(
            &adapter.pinned,
            &adapter.attestation_generation,
            first,
            &identity,
        ),
        Err(ProviderError::UnsupportedCapability(
            crate::providers::capabilities::ProviderCapability::BuildLaunch
        ))
    ));
    assert!(require_attestation(
        &adapter.pinned,
        &adapter.attestation_generation,
        second,
        &identity,
    )
    .is_ok());
}

#[tokio::test]
async fn codex_bind_first_rechecks_current_generation_atomically() {
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let registry = Arc::new(CodexHookRegistry::default());
    let task = TaskId::new();
    let agent = AgentSessionId::new();
    let mut launch = launch_with(
        &adapter,
        fixture_executable(),
        None,
        &registry,
        task,
        agent,
        process_root(26),
    )
    .unwrap();
    let replacement = issue_permit(&registry, task, agent, process_root(27));

    assert_eq!(
        launch.authority.bind_first(session_id()),
        Err(CodexIdentityError::Rejected)
    );
    drop(replacement);
}

#[tokio::test]
async fn codex_replaced_same_identity_cannot_keep_stale_registered_state() {
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let registry = Arc::new(CodexHookRegistry::default());
    let task = TaskId::new();
    let agent = AgentSessionId::new();
    let first = launch_with(
        &adapter,
        fixture_executable(),
        None,
        &registry,
        task,
        agent,
        process_root(28),
    )
    .unwrap();
    let second = launch_with(
        &adapter,
        fixture_executable(),
        None,
        &registry,
        task,
        agent,
        process_root(29),
    )
    .unwrap();
    assert_eq!(
        adapter.semantic_launch_state(&fixture_executable()),
        CodexSemanticLaunchState::Registered
    );

    drop(second);
    assert_eq!(
        adapter.semantic_launch_state(&fixture_executable()),
        CodexSemanticLaunchState::DependencyUnavailable
    );
    drop(first);
}

#[tokio::test]
async fn codex_relay_observation_does_not_publish_before_adapter_admission() {
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let registry = Arc::new(CodexHookRegistry::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    registry.set_event_handler(Some(Arc::new(move |_registration, event| {
        sink.lock().expect("event sink").push(event);
    })));
    let mut launch = launch_with(
        &adapter,
        fixture_executable(),
        None,
        &registry,
        TaskId::new(),
        AgentSessionId::new(),
        process_root(30),
    )
    .unwrap();

    let observation = launch.relay_ingest(loopback(), SESSION_START.as_bytes(), 1);
    assert_eq!(observation.status(), CodexRelayIngestStatus::Accepted);
    assert!(events.lock().expect("event sink").is_empty());

    assert!(matches!(
        launch.admit_ingest(observation, SESSION_START.as_bytes()),
        Ok(CodexAdmission::Bound(_))
    ));
    assert!(events
        .lock()
        .expect("event sink")
        .iter()
        .any(|event| matches!(event, CodexRegistryEvent::SessionStarted(_))));
}

#[tokio::test]
async fn codex_identity_cannot_rebind_to_a_different_session_id() {
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let registry = Arc::new(CodexHookRegistry::default());
    let mut launch = launch_with(
        &adapter,
        fixture_executable(),
        None,
        &registry,
        TaskId::new(),
        AgentSessionId::new(),
        process_root(14),
    )
    .unwrap();
    let first = launch.relay_ingest(loopback(), SESSION_START.as_bytes(), 1);
    assert!(matches!(
        launch.admit_ingest(first, SESSION_START.as_bytes()),
        Ok(CodexAdmission::Bound(_))
    ));
    let second_body = r#"{"hook_event_name":"SessionStart","session_id":"019f-different","cwd":"/fixture/workspace"}"#;
    let second = launch.relay_ingest(loopback(), second_body.as_bytes(), 2);
    assert_eq!(
        launch.admit_ingest(second, second_body.as_bytes()),
        Err(CodexIdentityError::AlreadyBound)
    );
    assert_eq!(
        launch.authority().bound_id().map(ProviderSessionId::as_str),
        Some(FIXTURE_SESSION_ID)
    );
}

#[tokio::test]
async fn codex_registry_permit_is_required_and_debug_redacted() {
    let registry = Arc::new(CodexHookRegistry::default());
    let agent = AgentSessionId::new();
    let permit = issue_permit(&registry, TaskId::new(), agent, process_root(1));
    let rendered = format!("{permit:?}");
    assert!(!rendered.contains("nonce"));
    assert!(!rendered.contains(&agent.to_string()));
    drop(permit);
}

#[tokio::test]
async fn codex_overlapping_launches_keep_per_registration_ownership() {
    let identity = fixture_executable();
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let registry = Arc::new(CodexHookRegistry::default());
    let first_task = TaskId::new();
    let first_agent = AgentSessionId::new();
    let first_root = process_root(2);
    let second_task = TaskId::new();
    let second_agent = AgentSessionId::new();
    let mut first = launch_with(
        &adapter,
        identity.clone(),
        None,
        &registry,
        first_task,
        first_agent,
        first_root,
    )
    .unwrap();
    let mut second = launch_with(
        &adapter,
        identity.clone(),
        None,
        &registry,
        second_task,
        second_agent,
        process_root(3),
    )
    .unwrap();
    drop(second);
    assert_eq!(
        adapter.semantic_launch_state(&identity),
        CodexSemanticLaunchState::Registered
    );
    let status = first.relay_ingest(loopback(), SESSION_START.as_bytes(), 1);
    assert_eq!(status.status(), CodexRelayIngestStatus::Accepted);
    assert!(matches!(
        first.admit_ingest(status, SESSION_START.as_bytes()),
        Ok(CodexAdmission::Bound(_))
    ));

    second = launch_with(
        &adapter,
        identity.clone(),
        None,
        &registry,
        first_task,
        first_agent,
        first_root,
    )
    .unwrap();
    assert!(matches!(
        admit_from_relay(&mut first, SESSION_START.as_bytes(), 2),
        Err(CodexIdentityError::Rejected | CodexIdentityError::Replay)
    ));
    let status = second.relay_ingest(loopback(), SESSION_START.as_bytes(), 1);
    assert!(matches!(
        second.admit_ingest(status, SESSION_START.as_bytes()),
        Ok(CodexAdmission::Bound(_))
    ));
}

#[tokio::test]
async fn codex_unregister_then_admit_is_rejected() {
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let registry = Arc::new(CodexHookRegistry::default());
    let mut launch = launch_with(
        &adapter,
        fixture_executable(),
        None,
        &registry,
        TaskId::new(),
        AgentSessionId::new(),
        process_root(4),
    )
    .unwrap();
    let nonce = nonce_from_spec(launch.spec());
    registry.unregister(&nonce);
    assert!(matches!(
        admit_from_relay(&mut launch, SESSION_START.as_bytes(), 1),
        Err(CodexIdentityError::Rejected)
    ));
    assert!(launch.authority().bound_id().is_none());
}

#[tokio::test]
async fn codex_missing_unknown_control_and_oversized_session_ids_are_rejected() {
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let registry = Arc::new(CodexHookRegistry::default());
    let mut launch = launch_with(
        &adapter,
        fixture_executable(),
        None,
        &registry,
        TaskId::new(),
        AgentSessionId::new(),
        process_root(5),
    )
    .unwrap();

    for (label, body) in [
        ("missing", SESSION_START_MISSING_ID),
        ("unknown", SESSION_START_UNKNOWN),
        ("control", SESSION_START_CONTROL),
    ] {
        let observation = launch.relay_ingest(loopback(), body.as_bytes(), 1);
        assert_eq!(
            observation.status(),
            CodexRelayIngestStatus::Accepted,
            "{label} ingest"
        );
        assert!(
            matches!(
                launch.admit_ingest(observation, body.as_bytes()),
                Err(CodexIdentityError::MissingSessionId | CodexIdentityError::Rejected)
            ),
            "{label}"
        );
        assert!(launch.authority().bound_id().is_none(), "{label}");
    }

    let oversized_id = "a".repeat(MAX_PROVIDER_SESSION_ID_BYTES + 1);
    let oversized = format!(
        r#"{{"hook_event_name":"SessionStart","session_id":"{oversized_id}","cwd":"/fixture/workspace"}}"#
    );
    assert!(matches!(
        admit_from_relay(&mut launch, oversized.as_bytes(), 1),
        Err(CodexIdentityError::MissingSessionId | CodexIdentityError::Rejected)
    ));
    assert!(launch.authority().bound_id().is_none());

    let deep =
        r#"{"hook_event_name":"SessionStart","session_id":"ok","a":{"b":{"c":{"d":{"e":1}}}}}"#;
    assert!(matches!(
        admit_from_relay(&mut launch, deep.as_bytes(), 1),
        Err(CodexIdentityError::Rejected)
    ));
}

#[tokio::test]
async fn codex_registration_drop_unregisters_and_cannot_bind() {
    let identity = fixture_executable();
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let registry = Arc::new(CodexHookRegistry::default());
    let nonce = {
        let launch = launch_with(
            &adapter,
            identity.clone(),
            None,
            &registry,
            TaskId::new(),
            AgentSessionId::new(),
            process_root(6),
        )
        .unwrap();
        assert_eq!(
            adapter.semantic_launch_state(&identity),
            CodexSemanticLaunchState::Registered
        );
        nonce_from_spec(launch.spec())
    };
    assert_eq!(
        registry
            .ingest(loopback(), &nonce, SESSION_START.as_bytes(), 1)
            .status(),
        CodexRelayIngestStatus::Rejected
    );
    assert_eq!(
        adapter.semantic_launch_state(&identity),
        CodexSemanticLaunchState::DependencyUnavailable
    );
    assert_eq!(
        adapter
            .last_capabilities(&identity)
            .unwrap()
            .semantic_events,
        CapabilitySupport::Unsupported
    );
}

#[tokio::test]
async fn codex_relay_restart_and_oversized_or_malformed_json_fail_closed() {
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let registry = Arc::new(CodexHookRegistry::default());
    let restart_agent = AgentSessionId::new();
    let restart_task = TaskId::new();
    let restart_root = process_root(7);
    let first = launch_with(
        &adapter,
        fixture_executable(),
        None,
        &registry,
        restart_task,
        restart_agent,
        restart_root,
    )
    .unwrap();
    let old_nonce = nonce_from_spec(first.spec());
    drop(first);
    let restarted = launch_with(
        &adapter,
        fixture_executable(),
        None,
        &registry,
        restart_task,
        restart_agent,
        restart_root,
    )
    .unwrap();
    assert_eq!(
        registry
            .ingest(loopback(), &old_nonce, SESSION_START.as_bytes(), 1)
            .status(),
        CodexRelayIngestStatus::Rejected
    );
    assert!(restarted.authority().bound_id().is_none());

    let huge = vec![b'x'; MAX_CODEX_HOOK_BODY_BYTES + 1];
    assert_eq!(
        restarted.relay_ingest(loopback(), &huge, 1).status(),
        CodexRelayIngestStatus::BodyTooLarge
    );
    assert_eq!(
        restarted.relay_ingest(loopback(), b"{", 1).status(),
        CodexRelayIngestStatus::Malformed
    );
    assert!(restarted.authority().bound_id().is_none());
}

#[tokio::test]
async fn codex_unknown_event_is_redacted_after_authenticated_correlation() {
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    let registry = Arc::new(CodexHookRegistry::default());
    let mut launch = launch_with(
        &adapter,
        fixture_executable(),
        None,
        &registry,
        TaskId::new(),
        AgentSessionId::new(),
        process_root(8),
    )
    .unwrap();
    let observation = launch.relay_ingest(loopback(), UNKNOWN_EVENT.as_bytes(), 1);
    assert_eq!(observation.status(), CodexRelayIngestStatus::Accepted);
    match launch
        .admit_ingest(observation, UNKNOWN_EVENT.as_bytes())
        .expect("partial")
    {
        CodexAdmission::Partial { diagnostic } => {
            let rendered = format!("{diagnostic:?}");
            assert!(!rendered.contains("SomethingNew"));
            assert!(!rendered.contains("payload"));
            assert!(!rendered.contains(FIXTURE_SESSION_ID));
        }
        other => panic!("expected redacted partial, got {other:?}"),
    }
}

#[tokio::test]
async fn codex_managed_process_views_are_dependency_unavailable() {
    let adapter = probed(HELP, RESUME_HELP, LOGIN_CHATGPT).await;
    assert!(matches!(
        adapter.managed_process_views(),
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::SemanticEvents
        ))
    ));
}

#[tokio::test]
async fn codex_registry_observe_uses_adapter_login_status_seam() {
    let temp = tempfile::tempdir().unwrap();
    let executable = temp.path().join("codex");
    std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let runner = ScriptedProbeRunner::ok(VERSION, HELP, RESUME_HELP, LOGIN_CHATGPT);
    let mut registry = ProviderRegistry::new();
    registry
        .register(Arc::new(CodexAdapter::new(runner)))
        .unwrap();
    let observation = registry
        .observe(
            ProviderKind::Codex,
            &ProviderDiscoveryConfig {
                executable_override: Some(executable),
                path: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(observation.kind, ProviderKind::Codex);
    assert_eq!(
        observation.capabilities.auth_state,
        ProviderAuthState::AuthenticatedSubscription
    );
    assert_eq!(
        observation.capabilities.semantic_events,
        CapabilitySupport::Unsupported
    );
}

#[test]
fn rollout_filename_cannot_bind_provider_session_id() {
    assert_eq!(
        CodexAdapter::session_id_from_rollout_path(Path::new(
            "/fixture/codex/sessions/rollout-fixture.jsonl"
        )),
        Err(CodexIdentityError::RolloutInferenceForbidden)
    );
    assert_eq!(
        CodexAdapter::session_id_from_rollout_path(Path::new(
            r"C:\Users\u\.codex\sessions\2026\08\12\rollout-019f-should-not-bind.jsonl"
        )),
        Err(CodexIdentityError::RolloutInferenceForbidden)
    );
}
