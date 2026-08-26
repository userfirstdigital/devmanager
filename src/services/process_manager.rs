use crate::ai::claude_hooks::{
    prepare_correlated_claude_launch_overlay, quote_shell_argument, ClaudeCorrelationBinding,
    ClaudeHookRegistration, ClaudeHookRegistry, ClaudeHookRelayListener, ClaudeRegistryEvent,
    ClaudeShellKind,
};
use crate::ai::codex_hooks::{
    build_codex_hooks_command, codex_hook_argument_tokens, codex_supports_hooks,
    CodexHookRegistration, CodexHookRegistry, CodexHookRelayListener, CodexRegistryEvent,
    CodexSessionBinding,
};
use crate::browser::{
    browser_input_opens_prompt_boundary, codex_browser_config_overrides,
    prepare_claude_browser_overlay, BrowserAttachmentBroker, BrowserAttachmentSessionBinding,
    BrowserGatewayRegistrar, BrowserGatewayRegistration, BrowserPromptInput, BrowserProviderAccess,
    BrowserWorkspaceKey, BrowserWorkspaceSnapshot, ClaudeBrowserOverlay,
};
use crate::domain::id::{AgentSessionId, OperationId, ResourceId, TaskId};
#[cfg(test)]
use crate::domain::operation::ResourceFence;
use crate::domain::snapshot::{ProcessAccountingMemberSnapshot, ProcessMetricStatus};
use crate::models::{
    Project, ProjectFolder, RunCommand, SSHConnection, SessionTab, Settings, TabType,
};
use crate::notifications;
#[cfg(test)]
use crate::process::identity::ManagedProcessIdentity;
use crate::process::identity::ProcessOwner;
use crate::process::job::JobMemberObservation;
use crate::process::registry::ManagedProcessFence;
#[cfg(test)]
use crate::process::sampler::AccessibleProcess;
use crate::process::sampler::{
    InaccessibleProcess, ProcessMemberObservation, ProcessSampler, SamplerError, SamplingBudget,
};
use crate::process::teardown::{TeardownCompletionStore, MAX_MANAGED_TERMINAL_PORTS};
use crate::providers::host::ProviderHost;
use crate::providers::ProviderKind;
use crate::remote::presentation::{SemanticAdapterHealth, SemanticEventDraft, StableSessionKey};
use crate::remote::{ClaudeSemanticIdentity, CodexSemanticIdentity, RemoteActionResult};
use crate::services::ports_service::{PortInventory, PortStartReservation};
use crate::services::process_ops::{
    next_op_id, ProcessOp, ProcessOpCompletion, ProcessOpContext, ProcessOpKind, ProcessOpQueue,
    MAX_PROCESS_OP_BATCH_ITEMS,
};
use crate::services::{env_service, pid_file, platform_service};
use crate::state::AppState;
use crate::state::{
    AiIdleTransition, AiLaunchSpec, ProcessResourceLifecycle, ResourceMemoryMetric,
    ResourceMetricValueState, ResourceSnapshot, RuntimeState, ServerLaunchSpec, SessionDimensions,
    SessionExitState, SessionKind, SessionRuntimeState, SessionStatus, SshLaunchSpec,
};
#[cfg(not(windows))]
use crate::terminal::session::ManagedProcessObservationQuery;
#[cfg(windows)]
use crate::terminal::session::ManagedResourceSamplePublication;
use crate::terminal::session::{
    bash_shell_args, preferred_windows_bash_program, ManagedProcessObservationCapture,
    TerminalBackend, TerminalLaunchAuthority, TerminalModeSnapshot, TerminalScreenSnapshot,
    TerminalSession, TerminalSessionView,
};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::Sender,
    Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const AI_SESSION_ATTACH_GRACE_WINDOW: Duration = Duration::from_secs(30);
const MAX_RESTART_HISTORY_BYTES: usize = 256 * 1024;
const MAX_PROCESS_OP_HOST_STRING_BYTES: usize = 32 * 1024;

/// Resource collection is a background projection, but it still needs a hard
/// per-tick ceiling so a large Job cannot monopolize the process worker.
const RESOURCE_SAMPLE_MAX_MEMBERS_PER_TICK: usize = 512;
const RESOURCE_SAMPLE_TICK_BUDGET: Duration = Duration::from_millis(40);

#[derive(Debug, Default)]
struct ManagedJobObservationSnapshot {
    capture: Option<ManagedProcessObservationCapture>,
    managed_process_fence: Option<ManagedProcessFence>,
    members: Option<Vec<JobMemberObservation>>,
    error: Option<String>,
}

impl ManagedJobObservationSnapshot {
    fn members(&self) -> Option<&[JobMemberObservation]> {
        self.members.as_deref()
    }

    fn fence(&self) -> Option<&ManagedProcessFence> {
        self.capture
            .as_ref()
            .map(ManagedProcessObservationCapture::fence)
            .or(self.managed_process_fence.as_ref())
    }
}

/// A bounded, in-memory source snapshot used by deterministic process-accounting
/// tests. The Job members are captured through the real Job API before the
/// 40 ms projection tick; metric observations and labels are then supplied as
/// immutable input so a slow Windows identity query cannot make the test race.
#[derive(Debug, Default)]
struct ResourceSamplingSource {
    sessions: HashMap<String, ResourceSamplingSession>,
    #[cfg(test)]
    before_direct_publication_delay: Option<Duration>,
}

#[derive(Debug, Clone)]
struct ResourceSamplingSession {
    managed_process_fence: Option<ManagedProcessFence>,
    job_members: Vec<JobMemberObservation>,
    member_observations: Vec<ProcessMemberObservation>,
    metadata: HashMap<u32, ProcessProjectionMetadata>,
}

pub(crate) fn ai_session_needs_restore(
    session: Option<&SessionRuntimeState>,
    session_attached: bool,
    now: Instant,
) -> bool {
    let Some(session) = session else {
        return true;
    };

    if session.session_kind.is_ai() && !session_attached {
        if session.status == SessionStatus::Starting {
            return false;
        }
        if session.status == SessionStatus::Running
            && session.started_at.is_some_and(|started_at| {
                now.saturating_duration_since(started_at) <= AI_SESSION_ATTACH_GRACE_WINDOW
            })
        {
            return false;
        }
    }

    !session.status.is_live() || !session_attached
}

pub struct ProcessManager {
    inner: Arc<ProcessManagerInner>,
    op_queue: Arc<ProcessOpQueue>,
    _claude_overlay_owner: Arc<ClaudeOverlayOwner>,
    handle_lifecycle: Arc<ProcessManagerHandleLifecycle>,
    /// Only user/application handles vote on the native host lifetime.
    /// Crate-internal operation facades borrow the already-live host and must
    /// never become the final shutdown caller on one of its own worker threads.
    shutdown_vote: bool,
}

#[derive(Debug)]
struct ProcessManagerHandleLifecycle {
    state: Mutex<ProcessManagerHandleLifecycleState>,
}

#[derive(Debug)]
struct ProcessManagerHandleLifecycleState {
    active_handles: usize,
    shutting_down: bool,
}

impl ProcessManagerHandleLifecycle {
    fn new() -> Self {
        Self {
            state: Mutex::new(ProcessManagerHandleLifecycleState {
                active_handles: 1,
                shutting_down: false,
            }),
        }
    }

    fn acquire(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.shutting_down {
            return Err("Process manager is shutting down.".to_string());
        }
        state.active_handles = state.active_handles.saturating_add(1);
        Ok(())
    }

    fn release(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(state.active_handles > 0);
        state.active_handles = state.active_handles.saturating_sub(1);
        if state.active_handles == 0 && !state.shutting_down {
            state.shutting_down = true;
            return true;
        }
        false
    }
}

impl Clone for ProcessManager {
    fn clone(&self) -> Self {
        if self.shutdown_vote {
            self.handle_lifecycle
                .acquire()
                .expect("a live ProcessManager handle must remain cloneable");
        }
        Self {
            inner: self.inner.clone(),
            op_queue: self.op_queue.clone(),
            _claude_overlay_owner: self._claude_overlay_owner.clone(),
            handle_lifecycle: self.handle_lifecycle.clone(),
            shutdown_vote: self.shutdown_vote,
        }
    }
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        if self.shutdown_vote && self.handle_lifecycle.release() {
            // Fence queued and in-flight port refresh callbacks before
            // shutting down workers and closing managed server sessions.
            bump_server_lifecycle_generation(&self.inner);
            shutdown_process_manager_workers(&self.inner);
        }
    }
}

#[derive(Clone)]
pub enum RemoteSessionEvent {
    Output {
        session_id: String,
        bytes: Vec<u8>,
        mode: TerminalModeSnapshot,
        screen: Option<TerminalScreenSnapshot>,
    },
    Runtime {
        session_id: String,
        runtime: SessionRuntimeState,
    },
    Removed {
        session_id: String,
    },
    Semantic {
        draft: SemanticEventDraft,
    },
    ClaudeSemantic {
        identity: ClaudeSemanticIdentity,
        draft: SemanticEventDraft,
    },
    ClaudeAdapterRegistered {
        identity: ClaudeSemanticIdentity,
    },
    ClaudeAdapterRemoved {
        identity: ClaudeSemanticIdentity,
    },
    CodexSemantic {
        identity: CodexSemanticIdentity,
        draft: SemanticEventDraft,
    },
    CodexAdapterRegistered {
        identity: CodexSemanticIdentity,
    },
    CodexAdapterRemoved {
        identity: CodexSemanticIdentity,
    },
    AdapterHealth {
        stable_session_key: StableSessionKey,
        health: SemanticAdapterHealth,
    },
}

type RemoteSessionEventHandler = Arc<dyn Fn(RemoteSessionEvent) + Send + Sync>;
type CodexHooksSupportProbe = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
#[cfg(test)]
type ClaudeSemanticPublicationTestHook = Arc<dyn Fn() + Send + Sync>;
#[cfg(test)]
type ProcessManagerBackgroundTestHook = Arc<dyn Fn() + Send + Sync>;
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoRestartWorkerTestPhase {
    BeforeQueueAdmission,
    AfterQueueLease,
    AfterEffect,
}
#[cfg(test)]
type AutoRestartWorkerTestHook = Arc<dyn Fn(AutoRestartWorkerTestPhase) + Send + Sync>;
#[cfg(test)]
type ProcessManagerServerSessionSpawnerTestHook = Arc<
    dyn Fn(&Arc<ProcessManagerInner>, &ServerLaunchSpec, SessionDimensions) -> Result<(), String>
        + Send
        + Sync,
>;

pub(crate) struct ProcessManagerInner {
    sessions: Mutex<HashMap<String, Arc<TerminalSession>>>,
    browser_attachment_broker: BrowserAttachmentBroker,
    runtime_state: Arc<RwLock<RuntimeState>>,
    runtime_revision: AtomicU64,
    /// Monotonic admission epoch for every queued server lifecycle operation.
    /// Port refresh fences read this value instead of trying to enumerate
    /// operation paths or sessions.
    server_lifecycle_generation: AtomicU64,
    observed_runtime_generations: Mutex<HashMap<String, u64>>,
    settings: RwLock<Settings>,
    terminal_backend: TerminalBackend,
    /// One coordinator owns every managed server start admission. UI,
    /// restore, remote, and auto-restart paths all share this inventory so a
    /// reservation cannot be released when an operation is merely queued.
    port_inventory: PortInventory,
    debug_enabled: bool,
    restart_backoffs: Mutex<HashMap<String, RestartBackoff>>,
    notification_sound: RwLock<Option<String>>,
    scrollback_lines: RwLock<usize>,
    remote_dirty_sessions: Arc<Mutex<BTreeSet<String>>>,
    remote_session_handler: RwLock<Option<RemoteSessionEventHandler>>,
    claude_hook_registry: Arc<ClaudeHookRegistry>,
    claude_hook_listener: Mutex<Option<ClaudeHookRelayListener>>,
    claude_adapter_generation: AtomicU64,
    claude_hook_sessions: Mutex<HashMap<String, ClaudeHookSession>>,
    #[cfg(test)]
    claude_semantic_publication_test_hook: RwLock<Option<ClaudeSemanticPublicationTestHook>>,
    claude_hook_temp_root: PathBuf,
    claude_overlay_owner: Mutex<Weak<ClaudeOverlayOwner>>,
    browser_gateway_registrar: RwLock<Option<BrowserGatewayRegistrar>>,
    browser_provider_sessions: Mutex<HashMap<String, BrowserProviderSession>>,
    browser_diagnostics: Mutex<HashMap<String, String>>,
    codex_hook_registry: Arc<CodexHookRegistry>,
    codex_hook_listener: Mutex<Option<CodexHookRelayListener>>,
    codex_hooks_support_probe: RwLock<CodexHooksSupportProbe>,
    codex_adapter_generation: AtomicU64,
    codex_adapter_registry: Mutex<CodexAdapterRegistry>,
    resource_samplers: Mutex<HashMap<String, ProcessSampler>>,
    background_stop: AtomicBool,
    background_thread: Mutex<Option<thread::JoinHandle<()>>>,
    auto_restart_workers: Mutex<Vec<thread::JoinHandle<()>>>,
    terminal_authority_issuer: TerminalAuthorityIssuer,
    service_launch_issuer: Arc<crate::services::launch_authority::ServiceLaunchIssuer>,
    configured_supervisor: Mutex<Option<crate::services::supervisor::ConfiguredServiceSupervisor>>,
    op_queue: Mutex<Weak<ProcessOpQueue>>,
    handle_lifecycle: Arc<ProcessManagerHandleLifecycle>,
    provider_host: ProviderHost,
    provider_runtime: Mutex<ProviderRuntimeBook>,
    provider_sessions: Mutex<Option<ProductionProviderSessionManager>>,
    provider_session_store_path: Option<PathBuf>,
    #[cfg(test)]
    background_test_hook: RwLock<Option<ProcessManagerBackgroundTestHook>>,
    #[cfg(test)]
    auto_restart_worker_test_hook: RwLock<Option<AutoRestartWorkerTestHook>>,
    #[cfg(test)]
    server_session_spawner_test_hook: RwLock<Option<ProcessManagerServerSessionSpawnerTestHook>>,
}

#[derive(Debug, Clone)]
struct ClaudeHookSession {
    registration: ClaudeHookRegistration,
    settings_path: PathBuf,
}

struct BrowserProviderSession {
    registrar: BrowserGatewayRegistrar,
    registration: BrowserGatewayRegistration,
    _claude_overlay: Option<ClaudeBrowserOverlay>,
}

struct ClaudeOverlayOwner {
    inner: Weak<ProcessManagerInner>,
    process_root: PathBuf,
}

impl Drop for ClaudeOverlayOwner {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            drain_claude_hook_sessions_inner(&inner);
        }
        remove_owned_claude_overlay_root(&self.process_root);
    }
}

fn claude_semantic_identity(
    pty_session_id: &str,
    session: &ClaudeHookSession,
) -> ClaudeSemanticIdentity {
    ClaudeSemanticIdentity {
        pty_session_id: pty_session_id.to_string(),
        stable_session_key: session.registration.stable_session_key.clone(),
        registration_generation: session.registration.generation,
    }
}

fn claude_semantic_identity_for_registration(
    inner: &ProcessManagerInner,
    registration: &ClaudeHookRegistration,
) -> Option<ClaudeSemanticIdentity> {
    inner.claude_hook_sessions.lock().ok().and_then(|sessions| {
        sessions
            .iter()
            .find(|(_, session)| session.registration == *registration)
            .map(|(session_id, session)| claude_semantic_identity(session_id, session))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexAdapterIdentity {
    stable_session_key: StableSessionKey,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexExactResumeLaunchBinding {
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    resource_id: ResourceId,
    runtime_generation: u64,
    provider_kind: ProviderKind,
    expected_provider_session_id: crate::domain::ProviderSessionId,
}

impl CodexExactResumeLaunchBinding {
    fn key(&self) -> (ResourceId, u64) {
        (self.resource_id, self.runtime_generation)
    }

    fn matches_live(&self, live: &ProviderLiveSession) -> bool {
        live.task_id == self.task_id
            && live.agent_session_id == self.agent_session_id
            && live.provider_kind == self.provider_kind
            && live.provider_session_id.as_ref() == Some(&self.expected_provider_session_id)
            && live.fence.resource().resource_id == self.resource_id
            && live.fence.resource().runtime_generation == self.runtime_generation
    }

    fn task_failure(&self) -> ProviderSessionFailure {
        ProviderSessionFailure {
            task_id: self.task_id,
            agent_session_id: self.agent_session_id,
            provider_kind: self.provider_kind,
            failure: crate::providers::session::ExactResumeFailure::ProviderRejected,
        }
    }
}

fn codex_semantic_identity(
    pty_session_id: &str,
    identity: &CodexAdapterIdentity,
) -> CodexSemanticIdentity {
    CodexSemanticIdentity {
        pty_session_id: pty_session_id.to_string(),
        stable_session_key: identity.stable_session_key.clone(),
        registration_generation: identity.generation,
    }
}

#[derive(Debug)]
enum CodexAdapterSession {
    Pending(CodexAdapterIdentity),
    Degraded(CodexAdapterIdentity),
    Running {
        identity: CodexAdapterIdentity,
        registration: CodexHookRegistration,
        activated: bool,
        exact_resume: Option<CodexExactResumeLaunchBinding>,
    },
}

impl CodexAdapterSession {
    fn identity(&self) -> &CodexAdapterIdentity {
        match self {
            Self::Pending(identity) | Self::Degraded(identity) => identity,
            Self::Running { identity, .. } => identity,
        }
    }

    fn registered_semantic_identity(&self, pty_session_id: &str) -> Option<CodexSemanticIdentity> {
        match self {
            Self::Running { identity, .. } => {
                Some(codex_semantic_identity(pty_session_id, identity))
            }
            Self::Pending(_) | Self::Degraded(_) => None,
        }
    }
}

#[derive(Debug, Default)]
struct CodexAdapterRegistry {
    sessions: HashMap<String, CodexAdapterSession>,
    latest_generations: HashMap<StableSessionKey, u64>,
}

impl CodexAdapterRegistry {
    fn is_current(&self, identity: &CodexAdapterIdentity) -> bool {
        self.latest_generations
            .get(&identity.stable_session_key)
            .is_some_and(|generation| *generation == identity.generation)
            && self
                .sessions
                .values()
                .any(|session| session.identity() == identity)
    }

    fn note_generation(&mut self, identity: &CodexAdapterIdentity) {
        let generation = self
            .latest_generations
            .entry(identity.stable_session_key.clone())
            .or_insert(identity.generation);
        *generation = (*generation).max(identity.generation);
    }

    fn remove_session(&mut self, session_id: &str) -> Option<CodexAdapterSession> {
        let removed = self.sessions.remove(session_id);
        if let Some(session) = removed.as_ref() {
            let stable_session_key = &session.identity().stable_session_key;
            if !self
                .sessions
                .values()
                .any(|candidate| &candidate.identity().stable_session_key == stable_session_key)
            {
                self.latest_generations.remove(stable_session_key);
            }
        }
        removed
    }
}

fn next_adapter_generation(counter: &AtomicU64) -> Option<u64> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |generation| {
            generation.checked_add(1)
        })
        .ok()
}

fn fence_and_remove_claude_hook_session(
    inner: &ProcessManagerInner,
    session_id: &str,
    expected: Option<&ClaudeHookRegistration>,
) -> Option<ClaudeHookSession> {
    let candidate = {
        let sessions = inner
            .claude_hook_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions
            .get(session_id)
            .filter(|session| expected.is_none_or(|expected| session.registration == *expected))
            .cloned()
    }?;

    // The registry's generation write gate waits for every already-validated
    // publication to finish. Keep the registration-to-PTY correlation in the
    // session map until that fence completes so those publications can still
    // resolve their exact semantic identity instead of failing open generically.
    inner
        .claude_hook_registry
        .unregister_registration(&candidate.registration);

    let removed = {
        let mut sessions = inner
            .claude_hook_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions
            .get(session_id)
            .is_some_and(|session| session.registration == candidate.registration)
            .then(|| sessions.remove(session_id))
            .flatten()
    }?;
    emit_remote_session_event(
        inner,
        RemoteSessionEvent::ClaudeAdapterRemoved {
            identity: claude_semantic_identity(session_id, &removed),
        },
    );
    let _ = std::fs::remove_file(&removed.settings_path);
    Some(removed)
}

#[derive(Debug, Clone)]
struct RestartBackoff {
    delay: Duration,
    last_crash: Instant,
}

#[derive(Debug, Clone, Default)]
pub struct AiRestoreReport {
    pub reattached: usize,
    pub relaunched: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SshRestoreReport {
    pub reattached: usize,
    pub recovered: usize,
    pub disconnected: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ManagedShutdownReport {
    pub requested_sessions: usize,
    pub forced_kill_pids: usize,
    pub remaining_live_sessions: usize,
    pub remaining_tracked_pids: usize,
}

static AI_SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);
static SSH_SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);
static CLAUDE_OVERLAY_OWNER_COUNTER: AtomicU64 = AtomicU64::new(1);

const DEFAULT_CLAUDE_COMMAND: &str = "npx -y @anthropic-ai/claude-code@latest";
const DEFAULT_CODEX_COMMAND: &str = "npx -y @openai/codex@latest";
const PROCESS_MANAGER_HELPER_JOIN_BUDGET: Duration = Duration::from_secs(5);
const MAX_AUTO_RESTART_WORKERS: usize = 256;
const MAX_TERMINAL_AUTHORITY_RESOURCES: usize = 1_024;
const MAX_PROVIDER_SESSION_FAILURES: usize = 128;
const PROVIDER_EXIT_RETRY_INITIAL: Duration = Duration::from_millis(250);
const PROVIDER_EXIT_RETRY_MAX: Duration = Duration::from_secs(30);

type ProductionProviderSessionManager = crate::providers::session::ProviderSessionManager<
    crate::services::provider_process_launcher::ProcessManagerProviderLauncher,
    crate::providers::session::SqliteProviderSessionStateStore,
>;

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessManager {
    pub fn new() -> Self {
        Self::new_with_optional_provider_session_store_path(None)
    }

    /// Construct the host-owned process runtime with provider session state
    /// anchored beneath the same resolved profile root as the kernel database.
    pub(crate) fn new_with_provider_session_store_path(path: PathBuf) -> Self {
        Self::new_with_optional_provider_session_store_path(Some(path))
    }

    fn new_with_optional_provider_session_store_path(
        provider_session_store_path: Option<PathBuf>,
    ) -> Self {
        let debug_enabled = debug_enabled();
        let handle_lifecycle = Arc::new(ProcessManagerHandleLifecycle::new());
        let claude_hook_registry = Arc::new(ClaudeHookRegistry::default());
        let codex_hook_registry = Arc::new(CodexHookRegistry::default());
        let claude_hook_temp_root = prepare_claude_overlay_process_root();
        let inner = Arc::new(ProcessManagerInner {
            sessions: Mutex::new(HashMap::new()),
            browser_attachment_broker: BrowserAttachmentBroker::default(),
            runtime_state: Arc::new(RwLock::new(RuntimeState::new(debug_enabled))),
            runtime_revision: AtomicU64::new(1),
            server_lifecycle_generation: AtomicU64::new(0),
            observed_runtime_generations: Mutex::new(HashMap::new()),
            settings: RwLock::new(Settings::default()),
            terminal_backend: TerminalBackend::PortablePtyFeedingAlacritty,
            port_inventory: PortInventory::new(),
            debug_enabled,
            restart_backoffs: Mutex::new(HashMap::new()),
            notification_sound: RwLock::new(None),
            scrollback_lines: RwLock::new(10_000),
            remote_dirty_sessions: Arc::new(Mutex::new(BTreeSet::new())),
            remote_session_handler: RwLock::new(None),
            claude_hook_registry: claude_hook_registry.clone(),
            claude_hook_listener: Mutex::new(None),
            claude_adapter_generation: AtomicU64::new(1),
            claude_hook_sessions: Mutex::new(HashMap::new()),
            #[cfg(test)]
            claude_semantic_publication_test_hook: RwLock::new(None),
            claude_hook_temp_root: claude_hook_temp_root.clone(),
            claude_overlay_owner: Mutex::new(Weak::new()),
            browser_gateway_registrar: RwLock::new(None),
            browser_provider_sessions: Mutex::new(HashMap::new()),
            browser_diagnostics: Mutex::new(HashMap::new()),
            codex_hook_registry: codex_hook_registry.clone(),
            codex_hook_listener: Mutex::new(None),
            codex_hooks_support_probe: RwLock::new(Arc::new(codex_supports_hooks)),
            codex_adapter_generation: AtomicU64::new(1),
            codex_adapter_registry: Mutex::new(CodexAdapterRegistry::default()),
            resource_samplers: Mutex::new(HashMap::new()),
            background_stop: AtomicBool::new(false),
            background_thread: Mutex::new(None),
            auto_restart_workers: Mutex::new(Vec::new()),
            terminal_authority_issuer: TerminalAuthorityIssuer::new(),
            service_launch_issuer: Arc::new(
                crate::services::launch_authority::ServiceLaunchIssuer::new(),
            ),
            configured_supervisor: Mutex::new(None),
            op_queue: Mutex::new(Weak::new()),
            handle_lifecycle: handle_lifecycle.clone(),
            provider_host: ProviderHost::stock()
                .expect("stock provider adapters register deterministically"),
            provider_runtime: Mutex::new(ProviderRuntimeBook::default()),
            provider_sessions: Mutex::new(None),
            provider_session_store_path,
            #[cfg(test)]
            background_test_hook: RwLock::new(None),
            #[cfg(test)]
            auto_restart_worker_test_hook: RwLock::new(None),
            #[cfg(test)]
            server_session_spawner_test_hook: RwLock::new(None),
        });
        let claude_overlay_owner = Arc::new(ClaudeOverlayOwner {
            inner: Arc::downgrade(&inner),
            process_root: claude_hook_temp_root,
        });
        if let Ok(mut owner) = inner.claude_overlay_owner.lock() {
            *owner = Arc::downgrade(&claude_overlay_owner);
        }

        let codex_registry_inner = Arc::downgrade(&inner);
        codex_hook_registry.set_event_handler(Some(Arc::new(move |registration, event| {
            let Some(inner) = codex_registry_inner.upgrade() else {
                return;
            };
            handle_codex_hook_registry_event(&inner, registration, event);
        })));

        let registry_inner = Arc::downgrade(&inner);
        claude_hook_registry.set_event_handler(Some(Arc::new(move |registration, event| {
            let Some(inner) = registry_inner.upgrade() else {
                return;
            };
            match event {
                ClaudeRegistryEvent::Semantic(draft) => {
                    let registry = inner.claude_hook_registry.clone();
                    registry.publish_if_current(&registration, || {
                        #[cfg(test)]
                        if let Some(hook) = inner
                            .claude_semantic_publication_test_hook
                            .read()
                            .ok()
                            .and_then(|hook| hook.clone())
                        {
                            hook();
                        }
                        let identity =
                            claude_semantic_identity_for_registration(&inner, &registration);
                        if let Some(identity) = identity {
                            emit_remote_session_event(
                                &inner,
                                RemoteSessionEvent::ClaudeSemantic { identity, draft },
                            );
                        } else {
                            // Correlation is an optimization. If tracking was
                            // lost, preserve the provider event rather than
                            // hiding it behind an uncertain match.
                            emit_remote_session_event(
                                &inner,
                                RemoteSessionEvent::Semantic { draft },
                            );
                        }
                    });
                }
                ClaudeRegistryEvent::SessionStarted {
                    provider_session_id,
                } => {
                    let registry = inner.claude_hook_registry.clone();
                    registry.publish_if_current(&registration, || {
                        if let Some(identity) =
                            claude_semantic_identity_for_registration(&inner, &registration)
                        {
                            bind_runtime_provider_session_id(
                                &inner,
                                &identity.pty_session_id,
                                provider_session_id,
                            );
                        }
                    });
                }
                ClaudeRegistryEvent::AdapterHealth {
                    stable_session_key,
                    health,
                } => {
                    let registry = inner.claude_hook_registry.clone();
                    registry.publish_if_current(&registration, || {
                        emit_remote_session_event(
                            &inner,
                            RemoteSessionEvent::AdapterHealth {
                                stable_session_key,
                                health,
                            },
                        );
                    });
                }
                ClaudeRegistryEvent::RegistrationDropped {
                    stable_session_key,
                    nonce,
                    generation,
                    was_latest,
                } => {
                    let removed_identities = {
                        let mut sessions = inner
                            .claude_hook_sessions
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let removed = sessions
                            .iter()
                            .filter(|(_, session)| {
                                session.registration.nonce == nonce
                                    && session.registration.generation == generation
                            })
                            .map(|(session_id, session)| {
                                claude_semantic_identity(session_id, session)
                            })
                            .collect::<Vec<_>>();
                        sessions.retain(|_, session| {
                            session.registration.nonce != nonce
                                || session.registration.generation != generation
                        });
                        removed
                    };
                    for identity in removed_identities {
                        emit_remote_session_event(
                            &inner,
                            RemoteSessionEvent::ClaudeAdapterRemoved { identity },
                        );
                    }
                    if was_latest {
                        let checked_key = stable_session_key.clone();
                        inner.claude_hook_registry.publish_if_not_superseded(
                            &checked_key,
                            generation,
                            || {
                                emit_remote_session_event(
                                    &inner,
                                    RemoteSessionEvent::AdapterHealth {
                                        stable_session_key,
                                        health: SemanticAdapterHealth::Degraded,
                                    },
                                );
                            },
                        );
                    }
                }
            }
        })));

        let op_queue = ProcessOpQueue::new(Arc::downgrade(&inner));
        if let Ok(mut slot) = inner.op_queue.lock() {
            *slot = Arc::downgrade(&op_queue);
        }

        let thread_handle = spawn_background_tasks(Arc::downgrade(&inner));
        if let Ok(mut handle_slot) = inner.background_thread.lock() {
            *handle_slot = Some(thread_handle);
        }

        Self {
            inner,
            op_queue,
            _claude_overlay_owner: claude_overlay_owner,
            handle_lifecycle,
            shutdown_vote: true,
        }
    }

    fn production_provider_session_store_path(
        &self,
    ) -> Result<PathBuf, crate::providers::session::ProviderSessionError> {
        if let Some(path) = self.inner.provider_session_store_path.as_ref() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    crate::providers::session::ProviderSessionError::StateStore(error.to_string())
                })?;
            }
            return Ok(path.clone());
        }
        let root = crate::persistence::app_config_dir().map_err(|error| {
            crate::providers::session::ProviderSessionError::StateStore(error.to_string())
        })?;
        std::fs::create_dir_all(&root).map_err(|error| {
            crate::providers::session::ProviderSessionError::StateStore(error.to_string())
        })?;
        Ok(root.join("provider-sessions.sqlite3"))
    }

    pub fn drain_process_op_completions(&self) -> Vec<ProcessOpCompletion> {
        self.op_queue.drain_completions()
    }

    /// Narrow host seam for the Task-owned terminal service. It mints the
    /// exact Task/resource/runtime-generation/action-epoch authority consumed
    /// by the one suspended PTY launch path; no raw Job or termination handle
    /// crosses this boundary.
    #[allow(dead_code)]
    pub(crate) fn issue_task_terminal_launch_authority(
        &self,
        task_id: TaskId,
        session_id: &str,
        ports: &[u16],
    ) -> Result<TerminalLaunchAuthority, String> {
        if ports.len() > MAX_MANAGED_TERMINAL_PORTS {
            return Err(format!(
                "terminal launch port set exceeds {MAX_MANAGED_TERMINAL_PORTS} entries"
            ));
        }
        self.inner.terminal_authority_issuer.issue(
            session_id,
            ProcessOwner::Task(task_id),
            ports.to_vec(),
        )
    }

    pub(crate) fn issue_exact_provider_terminal_authority(
        &self,
        session_id: &str,
        task_id: TaskId,
        resource_id: ResourceId,
        generation: u64,
        action_epoch: u64,
    ) -> Result<TerminalLaunchAuthority, String> {
        self.inner.terminal_authority_issuer.issue_exact(
            session_id,
            ProcessOwner::Task(task_id),
            resource_id,
            generation,
            action_epoch,
            Vec::new(),
        )
    }

    /// Narrow host seam: one shared issuer for the single configured-service
    /// supervisor lifecycle owned by this process manager.
    pub(crate) fn configured_service_launch_authority(
        &self,
    ) -> crate::services::launch_authority::HostManagedLaunchAuthority {
        crate::services::launch_authority::HostManagedLaunchAuthority::with_issuer(Arc::clone(
            &self.inner.service_launch_issuer,
        ))
    }

    /// Open (or reuse) the one configured-service supervisor lifecycle.
    /// Fresh authority/live maps are not created per call.
    pub(crate) fn ensure_configured_service_supervisor<'a>(
        &self,
        sources: impl IntoIterator<Item = crate::services::binding::ConfiguredServiceSource<'a>>,
        host_id: crate::services::model::HostId,
        now_ms: u64,
    ) -> Result<(), crate::services::supervisor::SupervisorError> {
        let mut guard = self
            .inner
            .configured_supervisor
            .lock()
            .map_err(|_| crate::services::supervisor::SupervisorError::TeardownFailed)?;
        if guard.is_some() {
            return Ok(());
        }
        let bindings = crate::services::binding::bind_configured_services(sources)
            .map_err(crate::services::supervisor::SupervisorError::from)?;
        let supervisor = crate::services::supervisor::ConfiguredServiceSupervisor::from_bindings(
            bindings,
            self.configured_service_launch_authority(),
            host_id,
            now_ms,
        )?;
        *guard = Some(supervisor);
        Ok(())
    }

    /// Observe Logs/Health for one Task through the owned configured-service
    /// supervisor. Exact Task-scoped services must match; host-scoped services
    /// remain visible. Mutating actions are rejected here.
    pub(crate) fn configured_service_observe_for_task(
        &self,
        action: crate::services::supervisor::SupervisorAction,
        service_id: &crate::services::model::ServiceId,
        fence: crate::services::model::AdmissionFence,
        task_id: crate::domain::TaskId,
    ) -> Result<
        crate::services::supervisor::SupervisorOutcome,
        crate::services::supervisor::SupervisorError,
    > {
        match action {
            crate::services::supervisor::SupervisorAction::Logs
            | crate::services::supervisor::SupervisorAction::Health => {}
            crate::services::supervisor::SupervisorAction::Start
            | crate::services::supervisor::SupervisorAction::Stop
            | crate::services::supervisor::SupervisorAction::Restart => {
                return Err(crate::services::supervisor::SupervisorError::Refused(
                    crate::services::supervisor::SupervisorRefusal::Other,
                ));
            }
        }
        let scope = self.configured_service_scope(service_id)?;
        match scope {
            crate::services::model::ServiceScope::Host => {}
            crate::services::model::ServiceScope::Task {
                task_id: scoped_task,
            } if scoped_task == task_id => {}
            crate::services::model::ServiceScope::Task { .. } => {
                return Err(crate::services::supervisor::SupervisorError::Refused(
                    crate::services::supervisor::SupervisorRefusal::Ownership,
                ));
            }
        }
        self.configured_service_control(
            action,
            service_id,
            fence,
            crate::services::model::AdmissionRequester::Task(task_id),
        )
    }

    /// Typed control path against the owned configured-service supervisor.
    pub(crate) fn configured_service_control(
        &self,
        action: crate::services::supervisor::SupervisorAction,
        service_id: &crate::services::model::ServiceId,
        fence: crate::services::model::AdmissionFence,
        requester: crate::services::model::AdmissionRequester,
    ) -> Result<
        crate::services::supervisor::SupervisorOutcome,
        crate::services::supervisor::SupervisorError,
    > {
        let mut guard = self
            .inner
            .configured_supervisor
            .lock()
            .map_err(|_| crate::services::supervisor::SupervisorError::TeardownFailed)?;
        let supervisor = guard
            .as_mut()
            .ok_or(crate::services::supervisor::SupervisorError::TeardownFailed)?;
        supervisor.handle(action, service_id, fence, requester)
    }

    /// Redacted projection for the owned configured-service supervisor.
    pub(crate) fn configured_service_snapshots(
        &self,
    ) -> Result<
        Vec<crate::services::health::RedactedServiceSnapshot>,
        crate::services::supervisor::SupervisorError,
    > {
        let mut guard = self
            .inner
            .configured_supervisor
            .lock()
            .map_err(|_| crate::services::supervisor::SupervisorError::TeardownFailed)?;
        let supervisor = guard
            .as_mut()
            .ok_or(crate::services::supervisor::SupervisorError::TeardownFailed)?;
        supervisor.pump_io();
        let mut snapshots = Vec::new();
        for service_id in supervisor.service_ids() {
            snapshots.push(supervisor.snapshot(&service_id)?);
        }
        Ok(snapshots)
    }

    /// Host-owned, redacted, task-scoped snapshots for the Task Cockpit.
    /// Foreign task-scoped services are excluded; host-scoped services remain.
    pub(crate) fn configured_service_snapshots_for_task(
        &self,
        task_id: crate::domain::TaskId,
    ) -> Result<
        crate::services::cockpit::TaskServiceCockpitProjection,
        crate::services::supervisor::SupervisorError,
    > {
        let snapshots = self.configured_service_snapshots()?;
        Ok(
            crate::services::cockpit::TaskServiceCockpitProjection::from_host_snapshots(
                task_id, &snapshots,
            ),
        )
    }

    /// Read one service scope from the owned supervisor for fail-closed
    /// requester selection. Does not expose command/env material.
    pub(crate) fn configured_service_scope(
        &self,
        service_id: &crate::services::model::ServiceId,
    ) -> Result<crate::services::model::ServiceScope, crate::services::supervisor::SupervisorError>
    {
        let guard = self
            .inner
            .configured_supervisor
            .lock()
            .map_err(|_| crate::services::supervisor::SupervisorError::TeardownFailed)?;
        let supervisor = guard
            .as_ref()
            .ok_or(crate::services::supervisor::SupervisorError::TeardownFailed)?;
        supervisor
            .catalog_definitions()
            .into_iter()
            .find(|definition| definition.id == *service_id)
            .map(|definition| definition.scope.clone())
            .ok_or_else(|| {
                crate::services::supervisor::SupervisorError::UnknownService(service_id.clone())
            })
    }

    /// Additive services panel projection from the owned supervisor.
    pub(crate) fn configured_services_panel(
        &self,
    ) -> Result<crate::ui::ServicesPanelProjection, crate::services::supervisor::SupervisorError>
    {
        let snapshots = self.configured_service_snapshots()?;
        let dependencies = self.configured_service_dependency_labels()?;
        Ok(crate::ui::project_services_panel(&snapshots, &dependencies))
    }

    /// Task-scoped services panel projection for the selected Task cockpit.
    pub(crate) fn configured_services_panel_for_task(
        &self,
        task_id: crate::domain::TaskId,
    ) -> Result<crate::ui::ServicesPanelProjection, crate::services::supervisor::SupervisorError>
    {
        let projection = self.configured_service_snapshots_for_task(task_id)?;
        let dependencies = self.configured_service_dependency_labels()?;
        let dependencies: Vec<_> = dependencies
            .into_iter()
            .filter(|(service_id, _)| {
                projection
                    .snapshots
                    .iter()
                    .any(|snapshot| snapshot.service_id == *service_id)
            })
            .collect();
        Ok(crate::ui::project_services_panel(
            &projection.snapshots,
            &dependencies,
        ))
    }

    fn configured_service_dependency_labels(
        &self,
    ) -> Result<
        Vec<(
            crate::services::model::ServiceId,
            Vec<crate::services::model::ServiceId>,
        )>,
        crate::services::supervisor::SupervisorError,
    > {
        let guard = self
            .inner
            .configured_supervisor
            .lock()
            .map_err(|_| crate::services::supervisor::SupervisorError::TeardownFailed)?;
        let supervisor = guard
            .as_ref()
            .ok_or(crate::services::supervisor::SupervisorError::TeardownFailed)?;
        Ok(supervisor.dependency_labels())
    }

    /// Legacy alias kept for call sites that still say "open"; reuses one lifecycle.
    pub(crate) fn open_configured_service_supervisor<'a>(
        &self,
        sources: impl IntoIterator<Item = crate::services::binding::ConfiguredServiceSource<'a>>,
        host_id: crate::services::model::HostId,
        now_ms: u64,
    ) -> Result<(), crate::services::supervisor::SupervisorError> {
        self.ensure_configured_service_supervisor(sources, host_id, now_ms)
    }

    pub fn port_inventory(&self) -> PortInventory {
        self.inner.port_inventory.clone()
    }

    pub fn submit_process_op(&self, op: ProcessOp) -> Result<u64, String> {
        self.op_queue.submit(op)
    }

    fn schedule_start_server(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        validate_process_op_host_string(command_id, "server command identity")?;
        self.validate_server_launch(app_state, command_id)?;
        let Some(launch) =
            self.prepare_start_server(app_state, command_id, dimensions, activate_tab)?
        else {
            return Ok(());
        };
        let op_id = next_op_id();
        self.op_queue.submit(ProcessOp::StartServer {
            op_id,
            launch,
            dimensions,
            activate: activate_tab,
            response,
        })?;
        Ok(())
    }

    fn schedule_restart_server(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        banner: &str,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        validate_process_op_host_string(command_id, "server command identity")?;
        validate_process_op_host_string(banner, "restart banner")?;
        self.validate_server_launch(app_state, command_id)?;
        let (launch, clear_logs) =
            self.prepare_restart_server(app_state, command_id, dimensions, banner)?;
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::RestartServer {
                op_id,
                launch,
                dimensions,
                banner: banner.to_string(),
                clear_logs,
                response,
            })
            .map(|_| ())
    }

    fn schedule_stop_server_and_wait(
        &self,
        command_id: &str,
        wait: Duration,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        validate_process_op_host_string(command_id, "server command identity")?;
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::StopServer {
                op_id,
                command_id: command_id.to_string(),
                wait,
                response,
            })
            .map(|_| ())
    }

    fn schedule_stop_all_servers(
        &self,
        wait: Duration,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let runtime = self.runtime_state();
        let mut command_ids = Vec::with_capacity(MAX_PROCESS_OP_BATCH_ITEMS);
        for command_id in runtime
            .sessions
            .values()
            .filter(|session| session.status.is_live())
            .filter_map(|session| session.command_id.as_deref())
            .take(MAX_PROCESS_OP_BATCH_ITEMS + 1)
        {
            validate_process_op_host_string(command_id, "server command identity")?;
            command_ids.push(command_id.to_string());
        }
        if command_ids.len() > MAX_PROCESS_OP_BATCH_ITEMS {
            return Err(format!(
                "Stop-all server batch exceeds {MAX_PROCESS_OP_BATCH_ITEMS} managed sessions."
            ));
        }
        for command_id in &command_ids {
            self.update_session_state(command_id, |state| {
                state.note_user_stop_request();
                state.status = SessionStatus::Stopping;
                state.mark_dirty();
            });
        }
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::StopAll {
                op_id,
                command_ids,
                wait,
                response,
            })
            .map(|_| ())
    }

    pub fn schedule_shutdown(&self, timeout: Duration) -> Result<u64, String> {
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::Shutdown { op_id, timeout })?;
        Ok(op_id)
    }

    pub fn enqueue_stop_server_and_wait(
        &self,
        command_id: &str,
        wait: Duration,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        self.schedule_stop_server_and_wait(command_id, wait, response)
    }

    pub fn enqueue_kill_process(
        &self,
        session_id: &str,
        pid: u32,
        fence: ManagedProcessFence,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::KillProcess {
                op_id,
                session_id: session_id.to_string(),
                pid,
                fence,
                response,
            })
            .map(|_| ())
    }

    pub fn enqueue_kill_process_tree(
        &self,
        session_id: &str,
        pid: u32,
        fence: ManagedProcessFence,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::KillProcessTree {
                op_id,
                session_id: session_id.to_string(),
                pid,
                fence,
                response,
            })
            .map(|_| ())
    }

    pub fn validate_server_launch(
        &self,
        app_state: &AppState,
        command_id: &str,
    ) -> Result<(), String> {
        let lookup = app_state
            .find_command(command_id)
            .ok_or_else(|| format!("Unknown command `{command_id}`"))?;
        if lookup.command.command.trim().is_empty() {
            return Err(format!("Server command `{command_id}` is empty"));
        }
        Ok(())
    }

    fn prepare_start_server(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
    ) -> Result<Option<ServerLaunchSpec>, String> {
        let lookup = app_state
            .find_command(command_id)
            .ok_or_else(|| format!("Unknown command `{command_id}`"))?;

        let project_id = lookup.project.id.clone();
        let command_id = lookup.command.id.clone();
        let command_label = lookup.command.label.clone();
        let command_auto_restart = lookup.command.auto_restart.unwrap_or(false);
        let session_id = command_id.clone();
        let runtime = self.runtime_state();
        if let Some(session) = runtime.sessions.get(&session_id) {
            if session.has_live_process() && self.get_session(&session_id).is_ok() {
                if activate_tab {
                    app_state.open_server_tab(
                        &project_id,
                        &command_id,
                        Some(command_label.clone()),
                    );
                    self.set_active_session(session_id);
                } else {
                    app_state.ensure_server_tab(
                        &project_id,
                        &command_id,
                        Some(command_label.clone()),
                    );
                }
                return Ok(None);
            }
        }

        let previous_active_session_id = (!activate_tab)
            .then(|| runtime.active_session_id.clone())
            .flatten();

        if activate_tab {
            self.set_active_session(session_id.clone());
        }

        let cwd = PathBuf::from(lookup.folder.folder_path.clone());
        let cwd = if cwd.is_dir() {
            cwd
        } else {
            PathBuf::from(lookup.project.root_path.clone())
        };

        self.ensure_runtime_entry(&session_id, cwd.clone(), dimensions);

        let env = build_command_env(lookup.folder, lookup.command);
        let (program, args) =
            build_server_launch_command(&app_state.config.settings, lookup.command);
        let launch_spec = ServerLaunchSpec {
            command_id: command_id.clone(),
            project_id: project_id.clone(),
            port: lookup.command.port,
            cwd: cwd.clone(),
            program: program.clone(),
            args: args.clone(),
            env: env.clone(),
            auto_restart: command_auto_restart,
            log_file_path: build_server_log_file_path(
                lookup.project,
                lookup.folder,
                lookup.command,
            ),
        };

        if activate_tab {
            app_state.open_server_tab(&project_id, &command_id, Some(command_label.clone()));
        } else {
            app_state.ensure_server_tab(&project_id, &command_id, Some(command_label.clone()));
        }

        self.update_session_state(&session_id, |state| {
            state.status = SessionStatus::Starting;
            state.cwd = cwd.clone();
            state.dimensions = dimensions;
            state.shell_program = program.clone();
            state.configure_server(launch_spec.clone());
            state.exit = None;
            state.mark_dirty();
        });

        if !activate_tab {
            self.restore_active_session(previous_active_session_id);
        }

        Ok(Some(launch_spec))
    }

    fn prepare_restart_server(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        banner: &str,
    ) -> Result<(ServerLaunchSpec, bool), String> {
        let lookup = app_state
            .find_command(command_id)
            .ok_or_else(|| format!("Unknown command `{command_id}`"))?;

        let project_id = lookup.project.id.clone();
        let command_id = lookup.command.id.clone();
        let command_label = lookup.command.label.clone();
        let command_auto_restart = lookup.command.auto_restart.unwrap_or(false);
        let clear_logs_on_restart = lookup.command.clear_logs_on_restart.unwrap_or(true);
        let cwd = PathBuf::from(lookup.folder.folder_path.clone());
        let cwd = if cwd.is_dir() {
            cwd
        } else {
            PathBuf::from(lookup.project.root_path.clone())
        };
        let env = build_command_env(lookup.folder, lookup.command);
        let (program, args) =
            build_server_launch_command(&app_state.config.settings, lookup.command);
        let launch_spec = ServerLaunchSpec {
            command_id: command_id.clone(),
            project_id: project_id.clone(),
            port: lookup.command.port,
            cwd: cwd.clone(),
            program: program.clone(),
            args: args.clone(),
            env: env.clone(),
            auto_restart: command_auto_restart,
            log_file_path: build_server_log_file_path(
                lookup.project,
                lookup.folder,
                lookup.command,
            ),
        };

        self.update_session_state(&command_id, |state| {
            state.status = SessionStatus::Stopping;
            state.mark_dirty();
        });
        self.set_active_session(command_id.clone());
        app_state.open_server_tab(&project_id, &command_id, Some(command_label));
        self.update_session_state(&command_id, |state| {
            state.status = SessionStatus::Starting;
            state.cwd = cwd.clone();
            state.dimensions = dimensions;
            state.shell_program = program.clone();
            state.configure_server(launch_spec.clone());
            state.exit = None;
            state.mark_dirty();
        });

        let _ = banner;
        Ok((launch_spec, clear_logs_on_restart))
    }

    fn schedule_spawn_ai(
        &self,
        launch: &AiLaunchSpec,
        session_id: &str,
        dimensions: SessionDimensions,
        activate: bool,
        response: Option<Sender<RemoteActionResult>>,
        attachment_binding: impl Into<Option<BrowserAttachmentSessionBinding>>,
    ) -> Result<(), String> {
        let _ = activate;
        let op_id = next_op_id();
        let attachment_binding = attachment_binding.into();
        let result = self.op_queue.submit(ProcessOp::SpawnAi {
            op_id,
            launch: launch.clone(),
            session_id: session_id.to_string(),
            dimensions,
            attachment_binding: attachment_binding.clone(),
            response,
        });
        if result.is_err() {
            unbind_attachment_if_matches(&self.inner, attachment_binding.as_ref());
        }
        result.map(|_| ())
    }

    fn schedule_restart_ai(
        &self,
        close_session_id: Option<String>,
        launch: AiLaunchSpec,
        session_id: String,
        dimensions: SessionDimensions,
        response: Option<Sender<RemoteActionResult>>,
        attachment_binding: impl Into<Option<BrowserAttachmentSessionBinding>>,
    ) -> Result<(), String> {
        let op_id = next_op_id();
        let attachment_binding = attachment_binding.into();
        let result = self.op_queue.submit(ProcessOp::RestartAi {
            op_id,
            close_session_id,
            launch,
            session_id: session_id.clone(),
            dimensions,
            attachment_binding: attachment_binding.clone(),
            response,
        });
        if result.is_err() {
            unbind_attachment_if_matches(&self.inner, attachment_binding.as_ref());
        }
        result.map(|_| ())
    }

    fn schedule_close_ai(
        &self,
        session_id: &str,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let op_id = next_op_id();
        let attachment_binding = self.inner.browser_attachment_broker.binding(session_id);
        let result = self.op_queue.submit(ProcessOp::CloseAi {
            op_id,
            session_id: session_id.to_string(),
            response,
        });
        if result.is_err() {
            unbind_attachment_if_matches(&self.inner, attachment_binding.as_ref());
        }
        result.map(|_| ())
    }

    fn schedule_start_ssh(
        &self,
        launch: SshLaunchSpec,
        session_id: String,
        dimensions: SessionDimensions,
        key_warning: Option<String>,
        activate: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let _ = activate;
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::StartSsh {
                op_id,
                launch,
                session_id: session_id.clone(),
                dimensions,
                key_warning,
                response,
            })
            .map(|_| ())
    }

    fn schedule_restart_ssh(
        &self,
        close_session_id: Option<String>,
        launch: SshLaunchSpec,
        session_id: String,
        dimensions: SessionDimensions,
        key_warning: Option<String>,
        activate: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let _ = activate;
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::RestartSsh {
                op_id,
                close_session_id,
                launch,
                session_id: session_id.clone(),
                dimensions,
                key_warning,
                response,
            })
            .map(|_| ())
    }

    fn schedule_close_ssh(
        &self,
        session_id: Option<String>,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::CloseSsh {
                op_id,
                session_id,
                response,
            })
            .map(|_| ())
    }

    pub fn runtime_state(&self) -> RuntimeState {
        self.inner
            .runtime_state
            .read()
            .map(|runtime| runtime.clone())
            .unwrap_or_default()
    }

    /// Return the exact current host-owned AI process session for one task tab.
    ///
    /// Browser gateway identity must be correlated to the launched runtime; it
    /// must never be reconstructed from task ids, timestamps, or transcript
    /// ordering. If duplicate live rows exist during a bounded transition, the
    /// most recently started runtime wins deterministically.
    pub fn live_ai_process_session_for_tab(&self, tab_id: &str) -> Option<String> {
        self.runtime_state()
            .sessions
            .into_values()
            .filter(|session| {
                session.session_kind.is_ai()
                    && session.status.is_live()
                    && session.tab_id.as_deref() == Some(tab_id)
            })
            .max_by(|left, right| {
                left.started_at
                    .cmp(&right.started_at)
                    .then_with(|| left.session_id.cmp(&right.session_id))
            })
            .map(|session| session.session_id)
    }

    pub fn runtime_revision(&self) -> u64 {
        self.inner.runtime_revision.load(Ordering::Relaxed)
    }

    pub(crate) fn server_lifecycle_generation(&self) -> u64 {
        self.inner
            .server_lifecycle_generation
            .load(Ordering::Acquire)
    }

    pub fn register_runtime_session(&self, session: SessionRuntimeState) {
        let session_id = session.session_id.clone();
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            runtime.sessions.insert(session_id.clone(), session);
        }
        bump_runtime_revision(&self.inner);
        emit_tracked_remote_runtime_snapshot(&self.inner, &session_id);
    }

    pub fn terminal_backend(&self) -> TerminalBackend {
        self.inner.terminal_backend
    }

    pub fn drain_remote_dirty_sessions(&self) -> Vec<String> {
        let Ok(mut dirty) = self.inner.remote_dirty_sessions.lock() else {
            return Vec::new();
        };
        let values = dirty.iter().cloned().collect();
        dirty.clear();
        values
    }

    pub fn debug_enabled(&self) -> bool {
        self.inner.debug_enabled
    }

    pub fn set_remote_session_handler(&self, handler: Option<RemoteSessionEventHandler>) {
        if let Ok(mut slot) = self.inner.remote_session_handler.write() {
            *slot = handler;
        }
    }

    pub fn set_browser_gateway_registrar(&self, registrar: Option<BrowserGatewayRegistrar>) {
        drain_browser_provider_sessions_inner(&self.inner);
        if let Ok(mut slot) = self.inner.browser_gateway_registrar.write() {
            *slot = registrar;
        }
    }

    pub fn browser_attachment_broker(&self) -> BrowserAttachmentBroker {
        self.inner.browser_attachment_broker.clone()
    }

    pub fn provider_host(&self) -> &ProviderHost {
        &self.inner.provider_host
    }

    pub fn provider_process_launcher(
        &self,
    ) -> crate::services::provider_process_launcher::ProcessManagerProviderLauncher {
        crate::services::provider_process_launcher::ProcessManagerProviderLauncher::new(
            self.clone(),
        )
    }

    pub fn start_adapter_sealed_provider_session<S>(
        &self,
        store: S,
        request: crate::providers::session::StartProviderSessionRequest,
    ) -> Result<
        crate::providers::session::ProviderRuntime,
        crate::providers::session::ProviderSessionError,
    >
    where
        S: crate::providers::session::ProviderSessionStateStore,
    {
        let mut manager = crate::providers::host::ProviderHost::session_manager(
            self.provider_process_launcher(),
            store,
        );
        manager.start(request)
    }

    /// Start through the durable production ProviderSessionManager owned by
    /// this ProcessManager. The manager and its SQLite state store live for
    /// the host lifetime; a temporary manager must never drop the lease after
    /// returning a runtime handle.
    pub fn start_production_provider_session(
        &self,
        request: crate::providers::session::StartProviderSessionRequest,
    ) -> Result<
        crate::providers::session::ProviderRuntime,
        crate::providers::session::ProviderSessionError,
    > {
        let mut slot = self.inner.provider_sessions.lock().map_err(|_| {
            crate::providers::session::ProviderSessionError::StateStore(
                "provider session manager lock poisoned".to_string(),
            )
        })?;
        if slot.is_none() {
            let store = crate::providers::session::SqliteProviderSessionStateStore::open(
                self.production_provider_session_store_path()?,
            )
            .map_err(crate::providers::session::ProviderSessionError::StateStore)?;
            *slot = Some(
                crate::providers::session::ProviderSessionManager::with_state_store(
                    self.provider_process_launcher(),
                    store,
                ),
            );
        }
        let start_result = slot
            .as_mut()
            .expect("production provider manager initialized")
            .start(request);
        let started_correlation = start_result
            .as_ref()
            .ok()
            .map(crate::providers::session::ProviderRuntime::correlation);
        drop(slot);
        if let Some(correlation) = started_correlation {
            reconcile_provider_session_start_after_launch(&self.inner, correlation);
        }
        start_result
    }

    /// Start one stock Claude/Codex/Cursor runtime through the provider-owned
    /// controller and the exact durable task/resource binding. The controller
    /// performs subscription admission and exact-resume validation before the
    /// ProcessManager creates the single native terminal process.
    pub fn start_production_stock_provider_session(
        &self,
        binding: crate::domain::AgentResourceBinding,
        agent: crate::domain::AgentSessionFacts,
        observation: &crate::providers::registry::ProviderObservation,
        input: Option<crate::providers::adapter::ProviderInput>,
        cwd: PathBuf,
        environment: std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
        mode: crate::providers::session::ProviderSessionStartMode,
    ) -> Result<
        crate::providers::session::ProviderRuntime,
        crate::providers::controller::StockProviderSessionError,
    > {
        let adapter = self
            .inner
            .provider_host
            .adapter(agent.provider_kind)
            .ok_or(
                crate::providers::controller::StockProviderSessionError::Adapter(
                    crate::providers::adapter::ProviderError::ProviderNotRegistered(
                        agent.provider_kind,
                    ),
                ),
            )?;
        let mut slot = self.inner.provider_sessions.lock().map_err(|_| {
            crate::providers::controller::StockProviderSessionError::Session(
                crate::providers::session::ProviderSessionError::StateStore(
                    "provider session manager lock poisoned".to_string(),
                ),
            )
        })?;
        if slot.is_none() {
            let store = crate::providers::session::SqliteProviderSessionStateStore::open(
                self.production_provider_session_store_path()
                    .map_err(crate::providers::controller::StockProviderSessionError::Session)?,
            )
            .map_err(|error| {
                crate::providers::controller::StockProviderSessionError::Session(
                    crate::providers::session::ProviderSessionError::StateStore(error),
                )
            })?;
            *slot = Some(
                crate::providers::session::ProviderSessionManager::with_state_store(
                    self.provider_process_launcher(),
                    store,
                ),
            );
        }
        let manager = slot
            .as_mut()
            .expect("production provider manager initialized");
        let start_result = crate::providers::controller::StockProviderSessionController::new()
            .start_with_resource_binding(
                manager,
                binding,
                agent,
                observation,
                adapter.as_ref(),
                input,
                cwd,
                environment,
                mode,
            );
        let latched = take_latched_codex_exact_resume_failure(
            &self.inner,
            (binding.resource_id, binding.runtime_generation),
        );
        if let Some(latched) = latched {
            queue_provider_session_failure(&self.inner, latched.task_failure());
            return match start_result {
                Ok(_) => {
                    manager
                        .settle_exact_resume_failure(latched.agent_session_id)
                        .map_err(
                            crate::providers::controller::StockProviderSessionError::Session,
                        )?;
                    Err(
                        crate::providers::controller::StockProviderSessionError::Session(
                            crate::providers::session::ProviderSessionError::ExactResumeFailed {
                                provider_session_id: latched.expected_provider_session_id,
                                failure:
                                    crate::providers::session::ExactResumeFailure::ProviderRejected,
                            },
                        ),
                    )
                }
                Err(error) => Err(error),
            };
        }
        let started_correlation = start_result
            .as_ref()
            .ok()
            .map(crate::providers::session::ProviderRuntime::correlation);
        drop(slot);
        if let Some(correlation) = started_correlation {
            reconcile_provider_session_start_after_launch(&self.inner, correlation);
        }
        start_result
    }

    pub(crate) fn provider_session_bindings(&self) -> Vec<ProviderSessionBinding> {
        let Ok(book) = self.inner.provider_runtime.lock() else {
            return Vec::new();
        };
        book.live
            .values()
            .filter(|live| live.provider_identity_confirmed && !live.exit_reported)
            .filter_map(|live| {
                Some(ProviderSessionBinding {
                    task_id: live.task_id,
                    agent_session_id: live.agent_session_id,
                    resource_id: live.fence.resource().resource_id,
                    provider_kind: live.provider_kind,
                    provider_session_id: live.provider_session_id.clone()?,
                    runtime_generation: live.fence.resource().runtime_generation,
                })
            })
            .collect()
    }

    pub(crate) fn drain_provider_session_failures(&self) -> Vec<ProviderSessionFailure> {
        let Ok(mut book) = self.inner.provider_runtime.lock() else {
            return Vec::new();
        };
        book.failures.drain(..).collect()
    }

    /// Close every provider session bound to one task. Missing manager state is
    /// success: a failed launch never created a live session.
    pub fn close_provider_task(
        &self,
        task_id: crate::domain::id::TaskId,
    ) -> Result<(), crate::providers::session::ProviderSessionError> {
        let mut slot = self.inner.provider_sessions.lock().map_err(|_| {
            crate::providers::session::ProviderSessionError::StateStore(
                "provider session manager lock poisoned".to_string(),
            )
        })?;
        match slot.as_mut() {
            Some(manager) => manager.close_task(task_id),
            None => Ok(()),
        }
    }

    pub fn admit_stock_provider_launch(
        &self,
        kind: ProviderKind,
        provider_session_id: Option<&str>,
    ) -> Result<crate::providers::host::HostAiLaunchAdmission, String> {
        self.inner
            .provider_host
            .admit_production_ai_session(kind, provider_session_id)
            .map_err(|error| error.to_string())
    }

    pub fn browser_diagnostic(&self, ai_tab_id: &str) -> Option<String> {
        self.inner
            .browser_diagnostics
            .lock()
            .ok()
            .and_then(|diagnostics| diagnostics.get(ai_tab_id).cloned())
    }

    fn set_browser_diagnostic(&self, ai_tab_id: &str, diagnostic: Option<String>) {
        if let Ok(mut diagnostics) = self.inner.browser_diagnostics.lock() {
            match diagnostic {
                Some(diagnostic) => {
                    diagnostics.insert(ai_tab_id.to_string(), diagnostic);
                }
                None => {
                    diagnostics.remove(ai_tab_id);
                }
            }
        }
    }

    fn prepare_browser_launch_for_session(
        &self,
        launch: &mut AiLaunchSpec,
        session_id: &str,
        mut initial_snapshot: BrowserWorkspaceSnapshot,
    ) -> Option<BrowserAttachmentSessionBinding> {
        if !matches!(launch.tool, SessionKind::Claude | SessionKind::Codex) {
            return None;
        }
        let workspace_key =
            match BrowserWorkspaceKey::new(launch.project_id.clone(), launch.tab_id.clone()) {
                Ok(workspace_key) => workspace_key,
                Err(error) => {
                    self.set_browser_diagnostic(
                        &launch.tab_id,
                        Some(format!("Browser tools unavailable: {error}")),
                    );
                    return None;
                }
            };
        self.inner
            .browser_attachment_broker
            .observe_workspace(workspace_key.clone(), &initial_snapshot);
        self.inner
            .browser_attachment_broker
            .overlay_snapshot(&workspace_key, &mut initial_snapshot);
        let attachment_binding = self
            .inner
            .browser_attachment_broker
            .bind_session(session_id, workspace_key.clone());
        let registrar = self
            .inner
            .browser_gateway_registrar
            .read()
            .ok()
            .and_then(|registrar| registrar.clone());
        let Some(registrar) = registrar else {
            return Some(attachment_binding);
        };
        let registration = match registrar.register_with_project_root(
            session_id,
            workspace_key,
            initial_snapshot,
            &launch.cwd,
        ) {
            Ok(registration) => registration,
            Err(error) => {
                self.set_browser_diagnostic(
                    &launch.tab_id,
                    Some(format!("Browser tools unavailable: {error}")),
                );
                return Some(attachment_binding);
            }
        };
        let claude_overlay = if launch.tool == SessionKind::Claude {
            match prepare_claude_browser_overlay(
                &self.inner.claude_hook_temp_root,
                session_id,
                &launch.startup_command,
                claude_shell_kind(&launch.shell_program),
                registration.access(),
            ) {
                Ok(overlay) => Some(overlay),
                Err(error) => {
                    registrar.revoke(&registration);
                    self.set_browser_diagnostic(
                        &launch.tab_id,
                        Some(format!("Browser tools unavailable: {error}")),
                    );
                    return Some(attachment_binding);
                }
            }
        } else {
            None
        };
        if let Some(overlay) = claude_overlay.as_ref() {
            launch.startup_command = overlay.startup_command().to_string();
        }
        let previous = self
            .inner
            .browser_provider_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                session_id.to_string(),
                BrowserProviderSession {
                    registrar: registrar.clone(),
                    registration,
                    _claude_overlay: claude_overlay,
                },
            );
        if let Some(previous) = previous {
            previous.registrar.revoke(&previous.registration);
        }
        self.set_browser_diagnostic(&launch.tab_id, None);
        Some(attachment_binding)
    }

    fn browser_environment(&self, session_id: &str) -> HashMap<String, String> {
        self.inner
            .browser_provider_sessions
            .lock()
            .ok()
            .and_then(|sessions| {
                sessions
                    .get(session_id)
                    .map(|session| session.registration.access().environment())
            })
            .unwrap_or_default()
    }

    fn browser_access(&self, session_id: &str) -> Option<BrowserProviderAccess> {
        self.inner
            .browser_provider_sessions
            .lock()
            .ok()
            .and_then(|sessions| {
                sessions
                    .get(session_id)
                    .map(|session| session.registration.access().clone())
            })
    }

    fn claude_hook_endpoint(&self) -> Result<String, String> {
        let mut listener = self
            .inner
            .claude_hook_listener
            .lock()
            .map_err(|_| "Claude hook listener lock is poisoned".to_string())?;
        if listener.is_none() {
            *listener = Some(ClaudeHookRelayListener::start(
                self.inner.claude_hook_registry.clone(),
            )?);
        }
        listener
            .as_ref()
            .map(|listener| listener.endpoint().to_string())
            .ok_or_else(|| "Claude hook listener did not start".to_string())
    }

    fn prepare_claude_launch_for_session(
        &self,
        launch: &mut AiLaunchSpec,
        session_id: &str,
        temp_root: &Path,
    ) {
        self.prepare_claude_launch_for_session_with_provider_session_id(
            launch, session_id, temp_root, None,
        );
    }

    fn prepare_claude_launch_for_session_with_provider_session_id(
        &self,
        launch: &mut AiLaunchSpec,
        session_id: &str,
        temp_root: &Path,
        expected_provider_session_id: Option<&str>,
    ) {
        if launch.tool != SessionKind::Claude {
            return;
        }
        let stable_session_key = StableSessionKey::from_tab(&launch.tab_id);
        let endpoint = match self.claude_hook_endpoint() {
            Ok(endpoint) => endpoint,
            Err(_) => {
                emit_remote_session_event(
                    &self.inner,
                    RemoteSessionEvent::AdapterHealth {
                        stable_session_key,
                        health: SemanticAdapterHealth::Degraded,
                    },
                );
                return;
            }
        };
        let executable = match std::env::current_exe() {
            Ok(executable) => executable,
            Err(_) => {
                emit_remote_session_event(
                    &self.inner,
                    RemoteSessionEvent::AdapterHealth {
                        stable_session_key,
                        health: SemanticAdapterHealth::Degraded,
                    },
                );
                return;
            }
        };
        let Some(generation) = next_adapter_generation(&self.inner.claude_adapter_generation)
        else {
            emit_remote_session_event(
                &self.inner,
                RemoteSessionEvent::AdapterHealth {
                    stable_session_key,
                    health: SemanticAdapterHealth::Degraded,
                },
            );
            return;
        };
        let expected_provider_session_id = match expected_provider_session_id
            .map(|raw| crate::domain::ProviderSessionId::new(raw.to_string()))
            .transpose()
        {
            Ok(expected) => expected,
            Err(_) => {
                emit_remote_session_event(
                    &self.inner,
                    RemoteSessionEvent::AdapterHealth {
                        stable_session_key,
                        health: SemanticAdapterHealth::Degraded,
                    },
                );
                return;
            }
        };
        let task_id = TaskId::parse(&launch.tab_id).unwrap_or_else(|_| TaskId::new());
        let agent_session_id =
            AgentSessionId::parse(session_id).unwrap_or_else(|_| AgentSessionId::new());
        let correlation = ClaudeCorrelationBinding::new(
            task_id,
            agent_session_id,
            generation,
            generation,
            ResourceId::new(),
        );
        let overlay = prepare_correlated_claude_launch_overlay(
            &self.inner.claude_hook_registry,
            stable_session_key.clone(),
            correlation,
            expected_provider_session_id,
            &launch.startup_command,
            claude_shell_kind(&launch.shell_program),
            &executable,
            &endpoint,
            temp_root,
            Instant::now(),
        );
        let health = overlay.health;
        if let (Some(registration), Some(settings_path)) =
            (overlay.registration, overlay.settings_path)
        {
            let session = ClaudeHookSession {
                registration,
                settings_path,
            };
            let identity = claude_semantic_identity(session_id, &session);
            let previous = self
                .inner
                .claude_hook_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(session_id)
                .map(|session| session.registration.clone());
            if let Some(previous) = previous {
                fence_and_remove_claude_hook_session(&self.inner, session_id, Some(&previous));
            }
            self.inner
                .claude_hook_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(session_id.to_string(), session);
            launch.startup_command = overlay.startup_command;
            emit_remote_session_event(
                &self.inner,
                RemoteSessionEvent::ClaudeAdapterRegistered { identity },
            );
        }
        emit_remote_session_event(
            &self.inner,
            RemoteSessionEvent::AdapterHealth {
                stable_session_key,
                health,
            },
        );
    }

    fn cleanup_claude_hook_session(&self, session_id: &str) {
        cleanup_claude_hook_session_inner(&self.inner, session_id);
    }

    pub fn drain_claude_hook_adapter(&self) {
        drain_claude_hook_sessions_inner(&self.inner);
        remove_owned_claude_overlay_root(&self.inner.claude_hook_temp_root);
    }

    pub fn drain_browser_provider_adapter(&self) {
        drain_browser_provider_sessions_inner(&self.inner);
    }

    #[cfg(test)]
    fn set_codex_hooks_support_probe_for_test(&self, probe: CodexHooksSupportProbe) {
        *self
            .inner
            .codex_hooks_support_probe
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = probe;
    }

    #[cfg(test)]
    pub(crate) fn accept_codex_hooks_for_test(&self) {
        self.set_codex_hooks_support_probe_for_test(Arc::new(|_| Ok(())));
    }

    fn prepare_codex_launch_for_session(
        &self,
        launch: &mut AiLaunchSpec,
        session_id: &str,
    ) -> HashMap<String, String> {
        if launch.tool != SessionKind::Codex {
            return HashMap::new();
        }
        let browser_access = self.browser_access(session_id);
        let browser_config = browser_access
            .as_ref()
            .map(codex_browser_config_overrides)
            .unwrap_or_default();
        let stable_session_key = StableSessionKey::from_tab(&launch.tab_id);
        let Some(generation) = next_adapter_generation(&self.inner.codex_adapter_generation) else {
            emit_remote_session_event(
                &self.inner,
                RemoteSessionEvent::AdapterHealth {
                    stable_session_key,
                    health: SemanticAdapterHealth::Degraded,
                },
            );
            return HashMap::new();
        };
        let identity = CodexAdapterIdentity {
            stable_session_key,
            generation,
        };
        let replaced = {
            let mut registry = self
                .inner
                .codex_adapter_registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry.note_generation(&identity);
            registry.sessions.insert(
                session_id.to_string(),
                CodexAdapterSession::Pending(identity.clone()),
            )
        };
        let replaced_identity = replaced
            .as_ref()
            .and_then(|session| session.registered_semantic_identity(session_id));
        drop(replaced);
        if let Some(identity) = replaced_identity {
            emit_remote_session_event(
                &self.inner,
                RemoteSessionEvent::CodexAdapterRemoved { identity },
            );
        }

        let probe = self
            .inner
            .codex_hooks_support_probe
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Err(error) = probe(&launch.startup_command) {
            eprintln!("Codex hooks tap unavailable for {session_id}: {error}");
            mark_codex_adapter_degraded(&self.inner, session_id, &identity);
            self.cleanup_browser_provider_session(session_id);
            self.set_browser_diagnostic(
                &launch.tab_id,
                Some(
                    "Browser tools unavailable because Codex launch preparation failed".to_string(),
                ),
            );
            return HashMap::new();
        }
        let prepared = self
            .codex_hook_endpoint()
            .and_then(|endpoint| {
                std::env::current_exe()
                    .map(|executable| (endpoint, executable))
                    .map_err(|error| format!("resolve DevManager executable: {error}"))
            })
            .and_then(|(endpoint, executable)| {
                let registration = self
                    .inner
                    .codex_hook_registry
                    .register(identity.stable_session_key.clone())?;
                build_codex_hooks_command(
                    &launch.startup_command,
                    &launch.shell_program,
                    &executable,
                    &endpoint,
                    &registration.nonce,
                    &browser_config,
                )
                .inspect_err(|_| {
                    self.inner
                        .codex_hook_registry
                        .unregister(&registration.nonce);
                })
                .map(|startup_command| (registration, startup_command))
            });
        let (registration, startup_command) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                eprintln!("Codex hooks launch preparation failed for {session_id}: {error}");
                mark_codex_adapter_degraded(&self.inner, session_id, &identity);
                self.cleanup_browser_provider_session(session_id);
                self.set_browser_diagnostic(
                    &launch.tab_id,
                    Some(
                        "Browser tools unavailable because Codex launch preparation failed"
                            .to_string(),
                    ),
                );
                return HashMap::new();
            }
        };
        let mut terminal_env = HashMap::new();
        if let Some(access) = browser_access.as_ref() {
            terminal_env.extend(access.environment());
        }

        let installed = {
            let mut registry = self
                .inner
                .codex_adapter_registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let is_current = registry.is_current(&identity)
                && registry
                    .sessions
                    .get(session_id)
                    .is_some_and(|session| session.identity() == &identity);
            if is_current {
                registry.sessions.insert(
                    session_id.to_string(),
                    CodexAdapterSession::Running {
                        identity: identity.clone(),
                        registration: registration.clone(),
                        activated: false,
                        exact_resume: None,
                    },
                );
                true
            } else {
                false
            }
        };
        if !installed {
            self.inner
                .codex_hook_registry
                .unregister(&registration.nonce);
            self.cleanup_browser_provider_session(session_id);
            self.set_browser_diagnostic(
                &launch.tab_id,
                Some(
                    "Browser tools unavailable because the Codex adapter was superseded"
                        .to_string(),
                ),
            );
            return HashMap::new();
        }
        emit_remote_session_event(
            &self.inner,
            RemoteSessionEvent::CodexAdapterRegistered {
                identity: codex_semantic_identity(session_id, &identity),
            },
        );
        launch.startup_command = startup_command;
        terminal_env
    }

    fn codex_hook_endpoint(&self) -> Result<String, String> {
        let mut listener = self
            .inner
            .codex_hook_listener
            .lock()
            .map_err(|_| "Codex hook listener lock is poisoned".to_string())?;
        if listener.is_none() {
            *listener = Some(CodexHookRelayListener::start(
                self.inner.codex_hook_registry.clone(),
            )?);
        }
        listener
            .as_ref()
            .map(|listener| listener.endpoint().to_string())
            .ok_or_else(|| "Codex hook listener did not start".to_string())
    }

    fn prepare_ai_terminal_environment(
        &self,
        launch: &mut AiLaunchSpec,
        session_id: &str,
    ) -> HashMap<String, String> {
        let mut terminal_environment = self.prepare_codex_launch_for_session(launch, session_id);
        terminal_environment.extend(self.browser_environment(session_id));
        terminal_environment
    }

    fn prepare_sealed_provider_adapter(
        &self,
        request: &crate::providers::session::ProviderRuntimeLaunchRequest,
        session_id: &str,
        program: &str,
        mut args: Vec<String>,
    ) -> Result<(Vec<String>, Option<AiLaunchSpec>), crate::providers::session::ProviderLaunchError>
    {
        use crate::providers::session::{ProviderLaunchError, ProviderLaunchMode};

        let task_key = request.correlation().task_id().to_string();
        let tool = match request.provider_kind() {
            ProviderKind::ClaudeCode => SessionKind::Claude,
            ProviderKind::Codex => SessionKind::Codex,
            ProviderKind::Cursor => return Ok((args, None)),
        };
        let original_command = std::iter::once(program)
            .chain(args.iter().map(String::as_str))
            .map(|token| quote_shell_argument(token, ClaudeShellKind::Cmd))
            .collect::<Vec<_>>()
            .join(" ");
        let stable_session_key = StableSessionKey::from_tab(&task_key);

        match request.provider_kind() {
            ProviderKind::ClaudeCode => {
                let endpoint = self
                    .claude_hook_endpoint()
                    .map_err(|_| ProviderLaunchError::BridgeUnavailable)?;
                let relay_executable =
                    std::env::current_exe().map_err(|_| ProviderLaunchError::BridgeUnavailable)?;
                let correlation = ClaudeCorrelationBinding::new(
                    request.correlation().task_id(),
                    request.correlation().agent_session_id(),
                    request.correlation().generation(),
                    request.correlation().action_epoch(),
                    request.resource_id(),
                );
                let expected_provider_session_id = match request.launch_spec().mode() {
                    ProviderLaunchMode::ResumeExact(id) => Some(id.clone()),
                    ProviderLaunchMode::NewConversation => None,
                };
                let overlay = prepare_correlated_claude_launch_overlay(
                    &self.inner.claude_hook_registry,
                    stable_session_key.clone(),
                    correlation,
                    expected_provider_session_id,
                    &original_command,
                    ClaudeShellKind::Cmd,
                    &relay_executable,
                    &endpoint,
                    &self.inner.claude_hook_temp_root,
                    Instant::now(),
                );
                let (Some(registration), Some(settings_path)) =
                    (overlay.registration, overlay.settings_path)
                else {
                    emit_remote_session_event(
                        &self.inner,
                        RemoteSessionEvent::AdapterHealth {
                            stable_session_key,
                            health: SemanticAdapterHealth::Degraded,
                        },
                    );
                    return Err(ProviderLaunchError::BridgeUnavailable);
                };
                args.push("--settings".to_string());
                args.push(settings_path.to_string_lossy().into_owned());
                let session = ClaudeHookSession {
                    registration,
                    settings_path,
                };
                if let Some(previous) = self
                    .inner
                    .claude_hook_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(session_id)
                    .map(|session| session.registration.clone())
                {
                    fence_and_remove_claude_hook_session(&self.inner, session_id, Some(&previous));
                }
                let identity = claude_semantic_identity(session_id, &session);
                self.inner
                    .claude_hook_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(session_id.to_string(), session);
                emit_remote_session_event(
                    &self.inner,
                    RemoteSessionEvent::ClaudeAdapterRegistered { identity },
                );
                emit_remote_session_event(
                    &self.inner,
                    RemoteSessionEvent::AdapterHealth {
                        stable_session_key,
                        health: overlay.health,
                    },
                );
            }
            ProviderKind::Codex => {
                let Some(generation) =
                    next_adapter_generation(&self.inner.codex_adapter_generation)
                else {
                    return Err(ProviderLaunchError::BridgeUnavailable);
                };
                let identity = CodexAdapterIdentity {
                    stable_session_key: stable_session_key.clone(),
                    generation,
                };
                let replaced = {
                    let mut registry = self
                        .inner
                        .codex_adapter_registry
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    registry.note_generation(&identity);
                    registry.sessions.insert(
                        session_id.to_string(),
                        CodexAdapterSession::Pending(identity.clone()),
                    )
                };
                if let Some(replaced) = replaced
                    .as_ref()
                    .and_then(|session| session.registered_semantic_identity(session_id))
                {
                    emit_remote_session_event(
                        &self.inner,
                        RemoteSessionEvent::CodexAdapterRemoved { identity: replaced },
                    );
                }
                let probe = self
                    .inner
                    .codex_hooks_support_probe
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                if probe(&original_command).is_err() {
                    mark_codex_adapter_degraded(&self.inner, session_id, &identity);
                    return Err(ProviderLaunchError::BridgeUnavailable);
                }
                let endpoint = self
                    .codex_hook_endpoint()
                    .map_err(|_| ProviderLaunchError::BridgeUnavailable)?;
                let relay_executable =
                    std::env::current_exe().map_err(|_| ProviderLaunchError::BridgeUnavailable)?;
                let registration = self
                    .inner
                    .codex_hook_registry
                    .register_expected(
                        stable_session_key.clone(),
                        match request.launch_spec().mode() {
                            ProviderLaunchMode::ResumeExact(id) => Some(id.clone()),
                            ProviderLaunchMode::NewConversation => None,
                        },
                    )
                    .map_err(|_| ProviderLaunchError::BridgeUnavailable)?;
                let hook_args =
                    codex_hook_argument_tokens(&relay_executable, &endpoint, &registration.nonce)
                        .inspect_err(|_| {
                            self.inner
                                .codex_hook_registry
                                .unregister(&registration.nonce);
                        })
                        .map_err(|_| ProviderLaunchError::BridgeUnavailable)?;
                let installed = {
                    let mut registry = self
                        .inner
                        .codex_adapter_registry
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if registry.is_current(&identity)
                        && registry
                            .sessions
                            .get(session_id)
                            .is_some_and(|session| session.identity() == &identity)
                    {
                        registry.sessions.insert(
                            session_id.to_string(),
                            CodexAdapterSession::Running {
                                identity: identity.clone(),
                                registration: registration.clone(),
                                activated: false,
                                exact_resume: match request.launch_spec().mode() {
                                    ProviderLaunchMode::ResumeExact(provider_session_id) => {
                                        Some(CodexExactResumeLaunchBinding {
                                            task_id: request.correlation().task_id(),
                                            agent_session_id: request
                                                .correlation()
                                                .agent_session_id(),
                                            resource_id: request.resource_id(),
                                            runtime_generation: request.correlation().generation(),
                                            provider_kind: request.provider_kind(),
                                            expected_provider_session_id: provider_session_id
                                                .clone(),
                                        })
                                    }
                                    ProviderLaunchMode::NewConversation => None,
                                },
                            },
                        );
                        true
                    } else {
                        false
                    }
                };
                if !installed {
                    self.inner
                        .codex_hook_registry
                        .unregister(&registration.nonce);
                    return Err(ProviderLaunchError::BridgeUnavailable);
                }
                args.extend(hook_args);
                emit_remote_session_event(
                    &self.inner,
                    RemoteSessionEvent::CodexAdapterRegistered {
                        identity: codex_semantic_identity(session_id, &identity),
                    },
                );
            }
            ProviderKind::Cursor => unreachable!("Cursor returned before adapter preparation"),
        }

        let startup_command = std::iter::once(program)
            .chain(args.iter().map(String::as_str))
            .map(|token| quote_shell_argument(token, ClaudeShellKind::Cmd))
            .collect::<Vec<_>>()
            .join(" ");
        Ok((
            args.clone(),
            Some(AiLaunchSpec {
                tab_id: task_key.clone(),
                project_id: task_key,
                tool,
                cwd: request.launch_spec().cwd().to_path_buf(),
                shell_program: program.to_string(),
                shell_args: args,
                startup_command,
            }),
        ))
    }

    fn cleanup_codex_adapter_session(&self, session_id: &str) {
        cleanup_codex_adapter_session_inner(&self.inner, session_id);
    }

    fn cleanup_browser_provider_session(&self, session_id: &str) {
        cleanup_browser_provider_session_inner(&self.inner, session_id);
    }

    fn cleanup_ai_adapters_for_session(&self, session_id: &str) {
        cleanup_ai_adapters_for_session_inner(&self.inner, session_id);
    }

    pub fn set_notification_sound(&self, sound_id: Option<String>) {
        if let Ok(mut notification_sound) = self.inner.notification_sound.write() {
            *notification_sound = sound_id;
        }
    }

    pub fn set_settings(&self, settings: Settings) {
        if let Ok(mut settings_slot) = self.inner.settings.write() {
            *settings_slot = settings;
        }
    }

    pub fn set_log_buffer_size(&self, lines: usize) {
        let lines = lines.max(100);
        if let Ok(mut scrollback_lines) = self.inner.scrollback_lines.write() {
            *scrollback_lines = lines;
        }
        if let Ok(sessions) = self.inner.sessions.lock() {
            for session in sessions.values() {
                session.set_scrollback_lines(lines);
            }
        }
    }

    fn log_buffer_size(&self) -> usize {
        self.inner
            .scrollback_lines
            .read()
            .map(|lines| *lines)
            .unwrap_or(10_000)
    }

    pub fn set_active_session(&self, session_id: impl Into<String>) {
        let session_id = session_id.into();
        let mut cleared_unseen_ready = false;
        let mut active_changed = false;
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            active_changed = runtime.active_session_id.as_deref() != Some(session_id.as_str());
            if active_changed {
                runtime.active_session_id = Some(session_id.clone());
            }
            if let Some(session) = runtime.sessions.get_mut(&session_id) {
                cleared_unseen_ready = session.unseen_ready;
                session.clear_unseen_ready();
            }
        }
        if active_changed || cleared_unseen_ready {
            bump_runtime_revision(&self.inner);
        }
        if cleared_unseen_ready {
            mark_remote_session_dirty(&self.inner, &session_id);
            emit_tracked_remote_runtime_snapshot(&self.inner, &session_id);
        }
    }

    pub fn spawn_shell_session(
        &self,
        session_id: impl Into<String>,
        cwd: &Path,
        dimensions: SessionDimensions,
        default_terminal: Option<crate::models::DefaultTerminal>,
        mac_terminal_profile: Option<crate::models::MacTerminalProfile>,
    ) -> Result<(), String> {
        let session_id = session_id.into();
        self.set_active_session(session_id.clone());

        if self.session_exists(&session_id) {
            return Ok(());
        }

        ensure_prior_session_teardown_settled(&self.inner, &session_id, Duration::from_secs(2))?;
        let authority = issue_host_terminal_authority(&self.inner, &session_id, Vec::new())?;

        match TerminalSession::spawn(
            session_id.clone(),
            cwd.to_path_buf(),
            dimensions,
            default_terminal,
            mac_terminal_profile,
            self.inner
                .settings
                .read()
                .map(|settings| settings.shell_integration_enabled)
                .unwrap_or(true),
            self.log_buffer_size(),
            self.inner.runtime_state.clone(),
            self.inner.debug_enabled,
            Some(session_change_notifier(
                self.inner.clone(),
                session_id.clone(),
            )),
            Some(session_output_notifier(
                self.inner.clone(),
                session_id.clone(),
            )),
            authority,
        ) {
            Ok(session) => {
                self.inner
                    .sessions
                    .lock()
                    .map_err(|_| "Session store poisoned".to_string())?
                    .insert(session_id, Arc::new(session));
                Ok(())
            }
            Err(error) => {
                self.update_session_state(&session_id, |state| {
                    state.cwd = cwd.to_path_buf();
                    state.dimensions = dimensions;
                    state.status = SessionStatus::Failed;
                    state.exit = Some(SessionExitState {
                        code: None,
                        signal: None,
                        closed_by_user: false,
                        summary: error.clone(),
                    });
                    state.mark_dirty();
                });
                Err(error)
            }
        }
    }

    pub fn write_to_session(&self, session_id: &str, text: &str) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.write_text(text)
    }

    pub fn write_bytes_to_session(&self, session_id: &str, bytes: &[u8]) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.write_bytes(bytes)
    }

    pub(crate) fn launch_sealed_provider_runtime(
        &self,
        request: &crate::providers::session::ProviderRuntimeLaunchRequest,
    ) -> Result<
        crate::process::registry::ProviderManagedProcessPermit,
        crate::providers::session::ProviderLaunchError,
    > {
        use crate::process::registry::{
            ProviderManagedProcessPermit, RegistryIssuedTerminalOwnership,
        };
        use crate::providers::session::ProviderLaunchError;
        use crate::providers::ProviderKind;
        use crate::state::SessionDimensions;
        use std::collections::HashMap;
        use std::time::Duration;

        if request.launch_spec().generation() == 0 || request.correlation().action_epoch() == 0 {
            return Err(ProviderLaunchError::ZeroProcessId);
        }
        // Cursor's stock CLI can be launched as a fresh interactive runtime,
        // but it does not expose an exact provider-session resume contract.
        // Keep that limitation typed and visible; never turn it into a fresh
        // conversation when the caller requested an exact resume.
        if request.provider_kind() == ProviderKind::Cursor
            && matches!(
                request.launch_spec().mode(),
                crate::providers::session::ProviderLaunchMode::ResumeExact(_)
            )
        {
            return Err(ProviderLaunchError::Unsupported);
        }
        // Stock Claude Code and Codex are subscription-backed CLIs in
        // DevManager. The adapter capability snapshot is the only accepted
        // auth proof; Unknown/API-key/auth-required evidence must fail before
        // a Job/PTY is created. An exact-resume request keeps the stronger
        // typed resume failure so callers cannot turn this into a fresh chat.
        if matches!(
            request.provider_kind(),
            ProviderKind::ClaudeCode | ProviderKind::Codex
        ) && request.capabilities().auth_state
            != crate::providers::capabilities::ProviderAuthState::AuthenticatedSubscription
        {
            return Err(match request.launch_spec().mode() {
                crate::providers::session::ProviderLaunchMode::ResumeExact(_) => {
                    ProviderLaunchError::ExactResumeFailed(
                        crate::providers::session::ExactResumeFailure::AuthRequired,
                    )
                }
                crate::providers::session::ProviderLaunchMode::NewConversation => {
                    ProviderLaunchError::AuthenticationRequired
                }
            });
        }
        #[cfg(not(windows))]
        {
            let _ = request;
            return Err(ProviderLaunchError::Unsupported);
        }
        #[cfg(windows)]
        {
            let session_id = format!("provider-{}", request.terminal_id());
            if !request.launch_spec().executable().is_native() {
                return Err(ProviderLaunchError::SpawnFailed);
            }
            if let Some(dependency) = request.launch_spec().runtime_dependency() {
                dependency
                    .validate_current()
                    .map_err(|_| ProviderLaunchError::SpawnFailed)?;
            }
            request
                .launch_spec()
                .executable()
                .validate_current()
                .map_err(|_| ProviderLaunchError::SpawnFailed)?;
            let program = request
                .launch_spec()
                .executable()
                .canonical_path()
                .to_str()
                .ok_or(ProviderLaunchError::SpawnFailed)?
                .to_string();
            let args = request
                .launch_spec()
                .create_process_arguments()
                .into_iter()
                .map(|argument| os_to_launch_string(&argument))
                .collect::<Result<Vec<_>, _>>()?;
            let env = request
                .launch_spec()
                .environment()
                .iter()
                .map(|(key, value)| Ok((os_to_launch_string(key)?, os_to_launch_string(value)?)))
                .collect::<Result<HashMap<String, String>, ProviderLaunchError>>()?;
            ensure_prior_session_teardown_settled(&self.inner, &session_id, Duration::from_secs(2))
                .map_err(|_| ProviderLaunchError::SpawnFailed)?;
            let (args, ai_launch) =
                self.prepare_sealed_provider_adapter(request, &session_id, &program, args)?;
            self.ensure_runtime_entry(
                &session_id,
                request.launch_spec().cwd().to_path_buf(),
                SessionDimensions::default(),
            );
            if let Some(ai_launch) = ai_launch {
                let resumed_provider_session_id = match request.launch_spec().mode() {
                    crate::providers::session::ProviderLaunchMode::ResumeExact(id) => {
                        Some(id.as_str().to_string())
                    }
                    crate::providers::session::ProviderLaunchMode::NewConversation => None,
                };
                self.update_session_state(&session_id, |state| {
                    state.status = SessionStatus::Starting;
                    state.cwd = ai_launch.cwd.clone();
                    state.shell_program = program.clone();
                    state.configure_ai(ai_launch.clone());
                    state.provider_session_id = resumed_provider_session_id.clone();
                    state.exit = None;
                });
            }
            let authority = self
                .issue_exact_provider_terminal_authority(
                    &session_id,
                    request.launch_spec().task_id(),
                    request.resource_id(),
                    request.launch_spec().generation(),
                    request.correlation().action_epoch(),
                )
                .map_err(|_| {
                    self.cleanup_ai_adapters_for_session(&session_id);
                    ProviderLaunchError::SpawnFailed
                })?;
            let session = TerminalSession::spawn_command(
                session_id.clone(),
                request.launch_spec().cwd().to_path_buf(),
                SessionDimensions::default(),
                program,
                args,
                env,
                self.inner
                    .scrollback_lines
                    .read()
                    .map(|lines| *lines)
                    .unwrap_or(10_000),
                None,
                self.inner.runtime_state.clone(),
                self.inner.debug_enabled,
                Some(session_change_notifier(
                    self.inner.clone(),
                    session_id.clone(),
                )),
                Some(session_output_notifier(
                    self.inner.clone(),
                    session_id.clone(),
                )),
                authority,
            )
            .map_err(|_| {
                self.cleanup_ai_adapters_for_session(&session_id);
                match request.launch_spec().mode() {
                    crate::providers::session::ProviderLaunchMode::ResumeExact(_) => {
                        ProviderLaunchError::ExactResumeFailed(
                            crate::providers::session::ExactResumeFailure::ProviderRejected,
                        )
                    }
                    crate::providers::session::ProviderLaunchMode::NewConversation => {
                        ProviderLaunchError::SpawnFailed
                    }
                }
            })?;
            let fence = match session.managed_process_fence() {
                Ok(Some(fence)) => fence,
                _ => {
                    let _ = session.close(false);
                    self.cleanup_ai_adapters_for_session(&session_id);
                    return Err(ProviderLaunchError::ProcessFenceMismatch);
                }
            };
            if fence.resource()
                != crate::domain::operation::ResourceFence::new(
                    request.resource_id(),
                    request.launch_spec().generation(),
                )
                || fence.owner() != ProcessOwner::Task(request.launch_spec().task_id())
                || fence.root().id().pid() == 0
                || fence.root().id().creation_time_100ns() == 0
                || fence.root().canonical_executable()
                    != request.launch_spec().executable().canonical_path()
            {
                let _ = session.close_managed_process_exact(&fence, false);
                self.cleanup_ai_adapters_for_session(&session_id);
                return Err(ProviderLaunchError::ProcessFenceMismatch);
            }
            if let Ok(mut sessions) = self.inner.sessions.lock() {
                sessions.insert(session_id.clone(), Arc::new(session));
            } else {
                let _ = session.close_managed_process_exact(&fence, false);
                self.cleanup_ai_adapters_for_session(&session_id);
                return Err(ProviderLaunchError::SpawnFailed);
            }
            if self
                .inner
                .provider_runtime
                .lock()
                .map(|mut book| {
                    book.live.insert(
                        (request.resource_id(), request.launch_spec().generation()),
                        ProviderLiveSession {
                            session_id: session_id.clone(),
                            fence: fence.clone(),
                            correlation: request.correlation(),
                            task_id: request.correlation().task_id(),
                            agent_session_id: request.correlation().agent_session_id(),
                            provider_kind: request.provider_kind(),
                            provider_session_id: match request.launch_spec().mode() {
                                crate::providers::session::ProviderLaunchMode::ResumeExact(id) => {
                                    Some(id.clone())
                                }
                                crate::providers::session::ProviderLaunchMode::NewConversation => {
                                    None
                                }
                            },
                            provider_identity_confirmed: false,
                            provider_identity_acceptance_started: false,
                            exit_reported: false,
                            settlement_kind: ProviderSettlementKind::ObserveExit,
                            settlement_failures: 0,
                            next_settlement_attempt: None,
                            failure_reported: false,
                        },
                    );
                })
                .is_err()
            {
                let _ = close_managed_process_exact(
                    &self.inner,
                    &session_id,
                    &fence,
                    fence.root().id().pid(),
                    true,
                );
                self.cleanup_ai_adapters_for_session(&session_id);
                return Err(ProviderLaunchError::BridgeUnavailable);
            }
            if provider_terminal_has_exited(&self.inner, &session_id) {
                reconcile_one_provider_terminal_exit(&self.inner, &session_id);
            }
            Ok(ProviderManagedProcessPermit::from_registry(
                fence,
                RegistryIssuedTerminalOwnership::new(session_id),
            ))
        }
    }

    pub(crate) fn stop_sealed_provider_runtime(
        &self,
        lease: &crate::process::registry::ProviderManagedProcessPermit,
    ) -> Result<
        crate::process::registry::JoinedActiveProcessZeroProof,
        crate::providers::session::ProviderLaunchError,
    > {
        use crate::process::registry::{JoinedActiveProcessZeroProof, RegistryIssuedZeroReceipt};
        use crate::providers::session::ProviderLaunchError;

        #[cfg(not(windows))]
        {
            let _ = lease;
            return Err(ProviderLaunchError::Unsupported);
        }
        #[cfg(windows)]
        {
            let key = (
                lease.fence().resource().resource_id,
                lease.fence().resource().runtime_generation,
            );
            let live = self
                .inner
                .provider_runtime
                .lock()
                .ok()
                .and_then(|book| book.live.get(&key).cloned());
            let Some(live) = live else {
                return Err(ProviderLaunchError::ActiveProcessZeroRequired);
            };
            if live.fence != *lease.fence() {
                return Err(ProviderLaunchError::ProcessFenceMismatch);
            }
            close_managed_process_exact(
                &self.inner,
                &live.session_id,
                lease.fence(),
                lease.process_id().pid(),
                true,
            )
            .map_err(|_| ProviderLaunchError::StopFailed)?;
            self.cleanup_ai_adapters_for_session(&live.session_id);
            if let Ok(mut book) = self.inner.provider_runtime.lock() {
                book.live.remove(&key);
            }
            Ok(JoinedActiveProcessZeroProof::from_registry(
                lease.fence().clone(),
                RegistryIssuedZeroReceipt::new(live.session_id),
            ))
        }
    }

    pub(crate) fn observe_sealed_provider_zero(
        &self,
        lease: &crate::process::registry::ProviderManagedProcessPermit,
    ) -> Result<
        Option<crate::process::registry::JoinedActiveProcessZeroProof>,
        crate::providers::session::ProviderLaunchError,
    > {
        use crate::providers::session::ProviderLaunchError;

        #[cfg(not(windows))]
        {
            let _ = lease;
            return Err(ProviderLaunchError::Unsupported);
        }
        #[cfg(windows)]
        {
            let key = (
                lease.fence().resource().resource_id,
                lease.fence().resource().runtime_generation,
            );
            let live = match self.inner.provider_runtime.lock() {
                Ok(book) => book.live.get(&key).cloned(),
                Err(_) => return Err(ProviderLaunchError::BridgeUnavailable),
            };
            let Some(live) = live else {
                return Ok(None);
            };
            if live.fence != *lease.fence() {
                return Err(ProviderLaunchError::ProcessFenceMismatch);
            }
            let session = self
                .get_session(&live.session_id)
                .map_err(|_| ProviderLaunchError::ActiveProcessZeroRequired)?;
            let deadline = Instant::now() + Duration::from_millis(250);
            let Some(observation) = session
                .managed_process_observations_until(deadline, 64)
                .map_err(|_| ProviderLaunchError::ActiveProcessZeroRequired)?
            else {
                return Err(ProviderLaunchError::ActiveProcessZeroRequired);
            };
            let (capture, members) = observation.into_parts();
            if capture.fence() != lease.fence() {
                return Err(ProviderLaunchError::ProcessFenceMismatch);
            }
            if !members
                .map_err(|_| ProviderLaunchError::ActiveProcessZeroRequired)?
                .is_empty()
            {
                return Ok(None);
            }
            Ok(Some(
                crate::process::registry::JoinedActiveProcessZeroProof::from_registry(
                    lease.fence().clone(),
                    crate::process::registry::RegistryIssuedZeroReceipt::new(live.session_id),
                ),
            ))
        }
    }

    pub(crate) fn retain_sealed_provider_runtime(
        &self,
        state: &crate::providers::session::ProviderSessionState,
        lease: crate::process::registry::ProviderManagedProcessPermit,
    ) -> Result<(), crate::providers::session::ProviderRecoveryHandoffFailure> {
        use crate::providers::session::{ProviderLaunchError, ProviderRecoveryHandoffFailure};

        if lease.fence().resource().runtime_generation != state.generation()
            || lease.fence().resource().resource_id != state.launch_spec().resource_id()
            || lease.fence().owner() != ProcessOwner::Task(state.task_id())
        {
            return Err(ProviderRecoveryHandoffFailure::new(
                ProviderLaunchError::ProcessFenceMismatch,
                lease,
            ));
        }
        match self.inner.provider_runtime.lock() {
            Ok(mut book) => {
                book.recovery.insert(
                    crate::providers::session::RecoveryKey::from_state(state),
                    lease,
                );
                Ok(())
            }
            Err(_) => Err(ProviderRecoveryHandoffFailure::new(
                ProviderLaunchError::BridgeUnavailable,
                lease,
            )),
        }
    }

    pub(crate) fn recover_sealed_provider_runtime(
        &self,
        state: &crate::providers::session::ProviderSessionState,
    ) -> Result<
        Option<crate::process::registry::ProviderManagedProcessPermit>,
        crate::providers::session::ProviderLaunchError,
    > {
        use crate::process::registry::{
            ProviderManagedProcessPermit, RegistryIssuedTerminalOwnership,
        };
        let key = crate::providers::session::RecoveryKey::from_state(state);
        if let Ok(mut book) = self.inner.provider_runtime.lock() {
            if let Some(lease) = book.recovery.remove(&key) {
                return Ok(Some(lease));
            }
            let live_key = (state.launch_spec().resource_id(), state.generation());
            if let Some(live) = book.live.get(&live_key).cloned() {
                if live.fence.resource().runtime_generation == state.generation()
                    && live.fence.owner() == ProcessOwner::Task(state.task_id())
                    && live.fence.root().id().pid() != 0
                {
                    return Ok(Some(ProviderManagedProcessPermit::from_registry(
                        live.fence,
                        RegistryIssuedTerminalOwnership::new(live.session_id),
                    )));
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn recover_sealed_provider_runtime_verified_absence(
        &self,
        state: &crate::providers::session::ProviderSessionState,
    ) -> Result<
        Option<crate::providers::session::ProviderRecoveryZeroSettlement>,
        crate::providers::session::ProviderLaunchError,
    > {
        use crate::process::sampler::ExactProcessIdentityStatus;
        use crate::providers::session::{ProviderLaunchError, ProviderRecoveryZeroSettlement};

        #[cfg(not(windows))]
        {
            let _ = state;
            return Err(ProviderLaunchError::Unsupported);
        }
        #[cfg(windows)]
        {
            let key = crate::providers::session::RecoveryKey::from_state(state);
            let live_key = (state.launch_spec().resource_id(), state.generation());
            let book = self
                .inner
                .provider_runtime
                .lock()
                .map_err(|_| ProviderLaunchError::BridgeUnavailable)?;
            if book.recovery.contains_key(&key) || book.live.contains_key(&live_key) {
                return Ok(None);
            }
            drop(book);

            let Some((process_id, executable)) = state.process_root_identity_parts() else {
                return Ok(None);
            };
            let expected =
                crate::process::identity::ManagedProcessIdentity::new(process_id, executable)
                    .map_err(|_| ProviderLaunchError::BridgeUnavailable)?;
            match ProcessSampler::observe_exact_process_identity(&expected) {
                ExactProcessIdentityStatus::Absent | ExactProcessIdentityStatus::Different => Ok(
                    Some(ProviderRecoveryZeroSettlement::from_verified_absence(state)),
                ),
                ExactProcessIdentityStatus::Present => Ok(None),
                ExactProcessIdentityStatus::Inaccessible => {
                    Err(ProviderLaunchError::BridgeUnavailable)
                }
            }
        }
    }

    pub(crate) fn write_sealed_provider_action(
        &self,
        fence: &ManagedProcessFence,
        identity: &crate::providers::input::ProviderInputDeliveryIdentity,
        action: &crate::domain::provider_input::ProviderInputAction,
        logical_bytes: &[u8],
    ) -> Result<(), crate::providers::input::ProviderInputDeliveryError> {
        use crate::providers::input::{provider_composer_submit_plan, ProviderInputDeliveryError};

        let expected = crate::providers::input::provider_input_action_bytes(action)
            .map_err(|_| ProviderInputDeliveryError::BytesMismatch)?;
        if expected.as_slice() != logical_bytes {
            return Err(ProviderInputDeliveryError::BytesMismatch);
        }
        let plan = provider_composer_submit_plan(identity.provider_kind, action)?;

        let key = (
            fence.resource().resource_id,
            fence.resource().runtime_generation,
        );
        let live = self
            .inner
            .provider_runtime
            .lock()
            .ok()
            .and_then(|book| book.live.get(&key).cloned())
            .ok_or(ProviderInputDeliveryError::SessionNotBound)?;
        if live.fence != *fence {
            return Err(ProviderInputDeliveryError::StaleFence);
        }
        if live.provider_session_id != identity.provider_session_id
            || identity.agent_session_id != live.agent_session_id
            || identity.task_id != live.task_id
            || identity.provider_kind != live.provider_kind
            || identity.runtime_generation != fence.resource().runtime_generation
        {
            return Err(ProviderInputDeliveryError::ProviderMismatch);
        }
        let session = self
            .get_session(&live.session_id)
            .map_err(|_| ProviderInputDeliveryError::SessionNotBound)?;
        #[cfg(windows)]
        {
            let current = session
                .managed_process_fence()
                .map_err(|_| ProviderInputDeliveryError::SessionNotBound)?
                .ok_or(ProviderInputDeliveryError::SessionNotBound)?;
            if current != *fence {
                return Err(ProviderInputDeliveryError::StaleFence);
            }
        }

        // Delays run only between physical writes and never while holding the
        // ProcessManager provider_runtime lock (released above after clone).
        let mut crossed_boundary = false;
        for step in plan.steps() {
            match session.write_provider_bytes(step.bytes()) {
                Ok(()) => {
                    crossed_boundary = true;
                    if let Some(delay) = step.delay_after() {
                        std::thread::sleep(delay);
                    }
                }
                Err(_) => {
                    return Err(if crossed_boundary {
                        ProviderInputDeliveryError::PostBoundaryFailure
                    } else {
                        ProviderInputDeliveryError::RuntimeAuthorityAbsent
                    });
                }
            }
        }
        Ok(())
    }

    pub(crate) fn write_sealed_provider_bytes(
        &self,
        fence: &ManagedProcessFence,
        identity: &crate::providers::input::ProviderInputDeliveryIdentity,
        bytes: &[u8],
    ) -> Result<(), crate::providers::input::ProviderInputDeliveryError> {
        // Legacy raw-byte entry point retained for transitional callers. Prefer
        // write_sealed_provider_action so submit is a distinct physical write.
        use crate::providers::input::ProviderInputDeliveryError;

        let key = (
            fence.resource().resource_id,
            fence.resource().runtime_generation,
        );
        let live = self
            .inner
            .provider_runtime
            .lock()
            .ok()
            .and_then(|book| book.live.get(&key).cloned())
            .ok_or(ProviderInputDeliveryError::SessionNotBound)?;
        if live.fence != *fence {
            return Err(ProviderInputDeliveryError::StaleFence);
        }
        if live.provider_session_id != identity.provider_session_id
            || identity.agent_session_id != live.agent_session_id
            || identity.task_id != live.task_id
            || identity.provider_kind != live.provider_kind
            || identity.runtime_generation != fence.resource().runtime_generation
        {
            return Err(ProviderInputDeliveryError::ProviderMismatch);
        }
        let session = self
            .get_session(&live.session_id)
            .map_err(|_| ProviderInputDeliveryError::SessionNotBound)?;
        #[cfg(windows)]
        {
            let current = session
                .managed_process_fence()
                .map_err(|_| ProviderInputDeliveryError::SessionNotBound)?
                .ok_or(ProviderInputDeliveryError::SessionNotBound)?;
            if current != *fence {
                return Err(ProviderInputDeliveryError::StaleFence);
            }
        }
        session
            .write_bytes(bytes)
            .map_err(|_| ProviderInputDeliveryError::RuntimeAuthorityAbsent)
    }

    pub(crate) fn live_provider_write_fence(
        &self,
        identity: &crate::providers::input::ProviderInputDeliveryIdentity,
    ) -> Result<ManagedProcessFence, crate::providers::input::ProviderInputDeliveryError> {
        use crate::providers::input::ProviderInputDeliveryError;
        let book = self
            .inner
            .provider_runtime
            .lock()
            .map_err(|_| ProviderInputDeliveryError::RuntimeAuthorityAbsent)?;
        let Some(live) = book.live.values().find(|live| {
            live.task_id == identity.task_id
                && live.agent_session_id == identity.agent_session_id
                && live.provider_kind == identity.provider_kind
                && live.provider_session_id == identity.provider_session_id
                && live.fence.resource().runtime_generation == identity.runtime_generation
        }) else {
            return Err(ProviderInputDeliveryError::SessionNotBound);
        };
        if live.fence.resource().runtime_generation != identity.runtime_generation {
            return Err(ProviderInputDeliveryError::StaleGeneration);
        }
        Ok(live.fence.clone())
    }

    pub fn paste_to_session(&self, session_id: &str, text: &str) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.paste_text(text)
    }

    pub fn write_user_text_to_session(&self, session_id: &str, text: &str) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        coordinate_user_origin_write(
            &self.inner.browser_attachment_broker,
            session_id,
            BrowserPromptInput::Text(text),
            |prefix| session.write_user_text(prefix, text),
        )
    }

    pub fn write_user_bytes_to_session(
        &self,
        session_id: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        coordinate_user_origin_write(
            &self.inner.browser_attachment_broker,
            session_id,
            BrowserPromptInput::RawBytes(bytes),
            |prefix| session.write_user_bytes(prefix, bytes),
        )
    }

    pub fn paste_user_text_to_session(&self, session_id: &str, text: &str) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        coordinate_user_origin_write(
            &self.inner.browser_attachment_broker,
            session_id,
            BrowserPromptInput::Paste(text),
            |prefix| session.paste_user_text(prefix, text),
        )
    }

    pub fn write_virtual_text(&self, session_id: &str, text: &str) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.write_virtual_text(text);
        Ok(())
    }

    pub fn clear_virtual_output(&self, session_id: &str) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.clear_virtual_output();
        self.update_session_state(session_id, |state| {
            state.display_offset = 0;
            state.mark_dirty();
        });
        Ok(())
    }

    pub fn note_server_interrupt(&self, session_id: &str) {
        self.update_session_state(session_id, |state| {
            if matches!(state.session_kind, SessionKind::Server)
                && state.status.is_live()
                && !state.interactive_shell
            {
                state.note_user_interrupt();
            }
        });
    }

    pub fn report_focus(&self, session_id: &str, focused: bool) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.report_focus(focused)
    }

    pub fn resize_session(
        &self,
        session_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<(), String> {
        let current_dimensions = self
            .runtime_state()
            .sessions
            .get(session_id)
            .map(|session| session.dimensions)
            .unwrap_or_default();

        if current_dimensions == dimensions {
            return Ok(());
        }

        let session = self.get_session(session_id)?;
        session.resize(dimensions)
    }

    pub fn scroll_session(&self, session_id: &str, delta_lines: i32) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.scroll(delta_lines)
    }

    pub fn scroll_session_to_offset(
        &self,
        session_id: &str,
        display_offset: usize,
    ) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.scroll_to_display_offset(display_offset)
    }

    pub fn scroll_session_to_buffer_line(
        &self,
        session_id: &str,
        buffer_line: usize,
    ) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.scroll_to_buffer_line(buffer_line)
    }

    pub fn session_screen_text(&self, session_id: &str) -> Result<String, String> {
        let session = self.get_session(session_id)?;
        Ok(session.screen_text())
    }

    pub fn session_scrollback_text(&self, session_id: &str) -> Result<String, String> {
        let session = self.get_session(session_id)?;
        Ok(session.scrollback_text())
    }

    pub fn session_replay_bytes(&self, session_id: &str) -> Result<Vec<u8>, String> {
        let session = self.get_session(session_id)?;
        Ok(session.replay_bytes())
    }

    pub fn search_session(
        &self,
        session_id: &str,
        query: &str,
        case_sensitive: bool,
        max_results: usize,
    ) -> Result<Vec<crate::terminal::session::TerminalSearchMatch>, String> {
        let session = self.get_session(session_id)?;
        Ok(session.search(query, case_sensitive, max_results))
    }

    pub fn close_session(&self, session_id: &str) -> Result<(), String> {
        self.close_session_with_reason(session_id, true)
    }

    fn close_session_with_reason(
        &self,
        session_id: &str,
        closed_by_user: bool,
    ) -> Result<(), String> {
        let attachment_binding = self.inner.browser_attachment_broker.binding(session_id);
        self.request_session_close(session_id, closed_by_user)?;
        self.finalize_settled_session(session_id)?;
        unbind_attachment_if_matches(&self.inner, attachment_binding.as_ref());
        Ok(())
    }

    pub fn close_tab(&self, app_state: &mut AppState, tab_id: &str) -> Result<(), String> {
        let Some(tab) = app_state.find_tab(tab_id).cloned() else {
            return Ok(());
        };

        match tab.tab_type {
            TabType::Server => {
                let command_id = tab.command_id.unwrap_or_else(|| tab.id.clone());
                let _ = self.enqueue_stop_server_and_wait(&command_id, Duration::ZERO, None);
                app_state.remove_tab(tab_id);
            }
            TabType::Claude | TabType::Codex => {
                self.close_ai_session(app_state, tab_id)?;
            }
            TabType::Ssh => {
                self.close_ssh_session(app_state, tab_id)?;
                app_state.remove_tab(tab_id);
            }
        }

        Ok(())
    }

    pub fn active_session(&self) -> Option<TerminalSessionView> {
        let runtime = self.runtime_state();
        let active_id = runtime.active_session_id?;
        let runtime_session = runtime.sessions.get(&active_id)?.clone();
        let session = self.get_session(&active_id).ok()?;

        Some(TerminalSessionView {
            runtime: runtime_session,
            screen: session.snapshot(),
        })
    }

    pub fn session_view_from_runtime(
        &self,
        runtime: &RuntimeState,
        session_id: &str,
    ) -> Option<TerminalSessionView> {
        let runtime_session = runtime.sessions.get(session_id)?.clone();
        let session = self.get_session(session_id).ok()?;

        Some(TerminalSessionView {
            runtime: runtime_session,
            screen: session.snapshot(),
        })
    }

    pub fn session_view(&self, session_id: &str) -> Option<TerminalSessionView> {
        let runtime = self.runtime_state();
        self.session_view_from_runtime(&runtime, session_id)
    }

    pub fn all_session_views(&self) -> HashMap<String, TerminalSessionView> {
        let runtime = self.runtime_state();
        let mut views = HashMap::new();
        for (session_id, runtime_session) in runtime.sessions.iter() {
            if let Ok(session) = self.get_session(session_id) {
                views.insert(
                    session_id.clone(),
                    TerminalSessionView {
                        runtime: runtime_session.clone(),
                        screen: session.snapshot(),
                    },
                );
            }
        }
        views
    }

    pub fn record_frame(&self, session_id: &str, render_duration: Duration) {
        let render_micros = render_duration.as_micros() as u64;
        match self.inner.runtime_state.try_write() {
            Ok(mut runtime) => {
                if let Some(session) = runtime.sessions.get_mut(session_id) {
                    session.record_frame(render_micros);
                }
            }
            Err(std::sync::TryLockError::Poisoned(error)) => {
                let mut runtime = error.into_inner();
                if let Some(session) = runtime.sessions.get_mut(session_id) {
                    session.record_frame(render_micros);
                }
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
    }

    pub fn start_ai_session(
        &self,
        app_state: &mut AppState,
        project_id: &str,
        tab_type: TabType,
        dimensions: SessionDimensions,
    ) -> Result<String, String> {
        self.start_ai_session_activate(app_state, project_id, tab_type, dimensions, true)
    }

    /// Same as `start_ai_session` but lets the caller decide whether to
    /// force the new tab to become the native UI's active tab. Remote
    /// clients should pass `activate = false` so a browser launching a new
    /// AI session doesn't yank the desktop window's focus onto a
    /// mid-spawn terminal — that path triggers a heavy GPUI render of a
    /// PTY being flooded with Claude Code's boot banner and stalls the
    /// main thread badly enough for Windows to mark the window
    /// "(Not Responding)".
    pub fn start_ai_session_activate(
        &self,
        app_state: &mut AppState,
        project_id: &str,
        tab_type: TabType,
        dimensions: SessionDimensions,
        activate: bool,
    ) -> Result<String, String> {
        self.start_ai_session_activate_with_response(
            app_state, project_id, tab_type, dimensions, activate, None,
        )
    }

    pub fn start_ai_session_activate_with_response(
        &self,
        app_state: &mut AppState,
        project_id: &str,
        tab_type: TabType,
        dimensions: SessionDimensions,
        activate: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<String, String> {
        if app_state.find_project(project_id).is_none() {
            return Err(format!("Unknown project `{project_id}`"));
        }
        let label = app_state.next_ai_label(project_id, tab_type.clone());
        let session_id = next_ai_session_id(&tab_type);
        let tab_id = session_id.clone();

        app_state.open_ai_tab_with_activation(
            project_id,
            tab_type,
            tab_id.clone(),
            session_id,
            Some(label),
            activate,
        );

        self.ensure_ai_session_for_tab_with_response(
            app_state, &tab_id, dimensions, activate, false, response,
        )
    }

    pub fn ensure_ai_session_for_tab(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        force_new_session: bool,
    ) -> Result<String, String> {
        self.ensure_ai_session_for_tab_with_response(
            app_state,
            tab_id,
            dimensions,
            activate_tab,
            force_new_session,
            None,
        )
    }

    pub fn ensure_ai_session_for_tab_with_response(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        force_new_session: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<String, String> {
        let tab = app_state
            .find_ai_tab(tab_id)
            .cloned()
            .ok_or_else(|| format!("Unknown AI tab `{tab_id}`"))?;

        let project = app_state
            .find_project(&tab.project_id)
            .cloned()
            .ok_or_else(|| format!("Unknown project `{}`", tab.project_id))?;

        let mut existing_session_to_close = None;
        if let Some(existing_session_id) = tab.pty_session_id.as_deref() {
            let existing_runtime = self
                .runtime_state()
                .sessions
                .get(existing_session_id)
                .cloned();
            let session_attached = self.get_session(existing_session_id).is_ok();
            if !force_new_session
                && !ai_session_needs_restore(
                    existing_runtime.as_ref(),
                    session_attached,
                    Instant::now(),
                )
            {
                if activate_tab {
                    let _ = app_state.select_tab(&tab.id);
                    if session_attached {
                        self.set_active_session(existing_session_id.to_string());
                    }
                }
                return Ok(existing_session_id.to_string());
            }
            existing_session_to_close = Some(existing_session_id.to_string());
        }

        let session_id = next_ai_session_id(&tab.tab_type);
        let mut launch =
            build_ai_launch_spec(&app_state.config.settings, &project, &tab, &session_id)?;
        let attachment_binding = self.prepare_browser_launch_for_session(
            &mut launch,
            &session_id,
            tab.browser_workspace.clone().unwrap_or_default(),
        );
        self.prepare_claude_launch_for_session_with_provider_session_id(
            &mut launch,
            &session_id,
            &self.inner.claude_hook_temp_root,
            tab.provider_session_id.as_deref(),
        );

        let _ = app_state.update_ai_tab_session(&tab.id, session_id.clone());
        if activate_tab {
            let _ = app_state.select_tab(&tab.id);
        }

        self.ensure_runtime_entry(&session_id, launch.cwd.clone(), dimensions);
        self.update_session_state(&session_id, |state| {
            state.status = SessionStatus::Starting;
            state.cwd = launch.cwd.clone();
            state.dimensions = dimensions;
            state.shell_program = launch.shell_program.clone();
            state.configure_ai(launch.clone());
            state.exit = None;
        });

        let schedule_result = if existing_session_to_close.is_some() {
            self.schedule_restart_ai(
                existing_session_to_close,
                launch.clone(),
                session_id.clone(),
                dimensions,
                response,
                attachment_binding,
            )
        } else {
            self.schedule_spawn_ai(
                &launch,
                &session_id,
                dimensions,
                activate_tab,
                response,
                attachment_binding,
            )
        };
        if let Err(error) = schedule_result {
            self.cleanup_ai_adapters_for_session(&session_id);
            return Err(error);
        }
        if activate_tab {
            self.set_active_session(session_id.clone());
        }
        Ok(session_id)
    }

    pub fn restart_ai_session(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<String, String> {
        self.restart_ai_session_activate(app_state, tab_id, dimensions, true)
    }

    pub fn validate_ai_restart(&self, app_state: &AppState, tab_id: &str) -> Result<(), String> {
        let tab = app_state
            .find_ai_tab(tab_id)
            .ok_or_else(|| format!("Unknown AI tab `{tab_id}`"))?;
        app_state
            .find_project(&tab.project_id)
            .ok_or_else(|| format!("Unknown project `{}`", tab.project_id))?;
        resolve_ai_startup_command(&app_state.config.settings, tab.tab_type.clone()).map(|_| ())
    }

    /// Same as `restart_ai_session` but lets the caller keep the native UI's
    /// current tab/session active. Remote-triggered AI restarts use this to
    /// recycle the PTY without yanking the desktop window onto the restarted
    /// terminal.
    pub fn restart_ai_session_activate(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
    ) -> Result<String, String> {
        self.restart_ai_session_activate_with_response(
            app_state,
            tab_id,
            dimensions,
            activate_tab,
            None,
        )
    }

    pub fn restart_ai_session_activate_with_response(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<String, String> {
        let existing_session_id = app_state
            .find_ai_tab(tab_id)
            .and_then(|tab| tab.pty_session_id.clone());

        let tab = app_state
            .find_ai_tab(tab_id)
            .cloned()
            .ok_or_else(|| format!("Unknown AI tab `{tab_id}`"))?;
        let project = app_state
            .find_project(&tab.project_id)
            .cloned()
            .ok_or_else(|| format!("Unknown project `{}`", tab.project_id))?;

        let session_id = next_ai_session_id(&tab.tab_type);
        let mut launch =
            build_ai_launch_spec(&app_state.config.settings, &project, &tab, &session_id)?;
        let attachment_binding = self.prepare_browser_launch_for_session(
            &mut launch,
            &session_id,
            tab.browser_workspace.clone().unwrap_or_default(),
        );
        self.prepare_claude_launch_for_session_with_provider_session_id(
            &mut launch,
            &session_id,
            &self.inner.claude_hook_temp_root,
            tab.provider_session_id.as_deref(),
        );

        let _ = app_state.update_ai_tab_session(&tab.id, session_id.clone());
        if activate_tab {
            let _ = app_state.select_tab(&tab.id);
        }

        self.ensure_runtime_entry(&session_id, launch.cwd.clone(), dimensions);
        self.update_session_state(&session_id, |state| {
            state.status = SessionStatus::Starting;
            state.cwd = launch.cwd.clone();
            state.dimensions = dimensions;
            state.shell_program = launch.shell_program.clone();
            state.configure_ai(launch.clone());
            state.exit = None;
        });

        if let Err(error) = self.schedule_restart_ai(
            existing_session_id,
            launch,
            session_id.clone(),
            dimensions,
            response,
            attachment_binding,
        ) {
            self.cleanup_ai_adapters_for_session(&session_id);
            return Err(error);
        }
        if activate_tab {
            self.set_active_session(session_id.clone());
        }
        Ok(session_id)
    }

    pub fn close_ai_session(&self, app_state: &mut AppState, tab_id: &str) -> Result<(), String> {
        self.close_ai_session_with_response(app_state, tab_id, None)
    }

    pub fn close_ai_session_with_response(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let attachment_workspace_key = app_state.browser_workspace_key(tab_id);
        let session_id = app_state
            .find_ai_tab(tab_id)
            .and_then(|tab| tab.pty_session_id.clone());

        app_state.remove_tab(tab_id);
        if let Some(workspace_key) = attachment_workspace_key {
            self.inner
                .browser_attachment_broker
                .retire_workspace(&workspace_key);
        }
        if let Some(session_id) = session_id {
            self.schedule_close_ai(&session_id, response)?;
        }
        Ok(())
    }

    pub fn reconcile_saved_ai_tabs(&self, app_state: &mut AppState) -> usize {
        let runtime = self.runtime_state();
        let mut recovered = Vec::new();
        let existing_ids: std::collections::HashSet<String> = app_state
            .open_tabs
            .iter()
            .map(|tab| tab.id.clone())
            .collect();

        for session in runtime.sessions.values() {
            if !session.session_kind.is_ai() || !session.status.is_live() {
                continue;
            }

            let Some(tab_id) = session.tab_id.as_ref() else {
                continue;
            };
            if existing_ids.contains(tab_id) {
                continue;
            }

            let tab_type = match session.session_kind {
                SessionKind::Claude => TabType::Claude,
                SessionKind::Codex => TabType::Codex,
                _ => continue,
            };
            let label = session
                .title
                .clone()
                .unwrap_or_else(|| default_ai_label(tab_type.clone()));

            recovered.push(SessionTab {
                id: tab_id.clone(),
                tab_type,
                project_id: session.project_id.clone().unwrap_or_default(),
                command_id: None,
                pty_session_id: Some(session.session_id.clone()),
                provider_session_id: session.provider_session_id.clone(),
                label: Some(label),
                ssh_connection_id: None,
                browser_workspace: None,
            });
        }

        app_state.merge_recovered_ai_tabs(recovered)
    }

    pub fn restore_ai_tabs(
        &self,
        app_state: &mut AppState,
        dimensions: SessionDimensions,
    ) -> AiRestoreReport {
        let mut report = AiRestoreReport::default();
        let active_tab_id = app_state.active_tab_id.clone();

        let saved_ai_tabs: Vec<String> = app_state.ai_tabs().map(|tab| tab.id.clone()).collect();
        for tab_id in saved_ai_tabs {
            let live_session_for_tab = self.runtime_state().sessions.values().find_map(|session| {
                (session.session_kind.is_ai()
                    && session.status.is_live()
                    && session.tab_id.as_deref() == Some(tab_id.as_str()))
                .then(|| session.session_id.clone())
            });
            if let Some(session_id) = live_session_for_tab {
                let _ = app_state.update_ai_tab_session(&tab_id, session_id);
                report.reattached += 1;
                continue;
            }

            let live_session = app_state
                .find_ai_tab(&tab_id)
                .and_then(|tab| tab.pty_session_id.as_deref())
                .and_then(|session_id| self.runtime_state().sessions.get(session_id).cloned())
                .map(|session| session.status.is_live())
                .unwrap_or(false);

            if live_session {
                report.reattached += 1;
                continue;
            }

            match self.ensure_ai_session_for_tab(app_state, &tab_id, dimensions, false, true) {
                Ok(_) => report.relaunched += 1,
                Err(_) => report.failed += 1,
            }
        }

        let recovered = self.reconcile_saved_ai_tabs(app_state);
        report.reattached += recovered;

        let next_active = active_tab_id
            .filter(|tab_id| app_state.find_tab(tab_id).is_some())
            .or_else(|| app_state.open_tabs.first().map(|tab| tab.id.clone()));
        if app_state.active_tab_id != next_active {
            app_state.active_tab_id = next_active;
            app_state.mark_dirty();
        }

        report
    }

    pub fn start_ssh_session(
        &self,
        app_state: &mut AppState,
        connection_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<String, String> {
        self.start_ssh_session_with_response(app_state, connection_id, dimensions, None)
    }

    pub fn start_ssh_session_with_response(
        &self,
        app_state: &mut AppState,
        connection_id: &str,
        dimensions: SessionDimensions,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<String, String> {
        let connection = app_state
            .find_ssh_connection(connection_id)
            .cloned()
            .ok_or_else(|| format!("Unknown SSH connection `{connection_id}`"))?;
        let project_id = app_state
            .find_ssh_tab_by_connection(connection_id)
            .map(|tab| tab.project_id.clone())
            .or_else(|| app_state.active_project().map(|project| project.id.clone()))
            .or_else(|| {
                app_state
                    .projects()
                    .first()
                    .map(|project| project.id.clone())
            })
            .unwrap_or_default();
        let tab_id = app_state.open_ssh_tab(&project_id, connection_id, Some(connection.label));

        self.ensure_ssh_session_for_tab_with_response(
            app_state, &tab_id, dimensions, true, false, response,
        )
    }

    pub fn ensure_ssh_session_for_tab(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        force_new_session: bool,
    ) -> Result<String, String> {
        self.ensure_ssh_session_for_tab_with_response(
            app_state,
            tab_id,
            dimensions,
            activate_tab,
            force_new_session,
            None,
        )
    }

    pub fn ensure_ssh_session_for_tab_with_response(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        force_new_session: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<String, String> {
        let tab = app_state
            .find_ssh_tab(tab_id)
            .cloned()
            .ok_or_else(|| format!("Unknown SSH tab `{tab_id}`"))?;
        let connection_id = tab
            .ssh_connection_id
            .clone()
            .ok_or_else(|| format!("SSH tab `{tab_id}` is missing a connection id"))?;
        let connection = app_state
            .find_ssh_connection(&connection_id)
            .cloned()
            .ok_or_else(|| format!("Unknown SSH connection `{connection_id}`"))?;

        if let Some(existing_session_id) = tab.pty_session_id.as_deref() {
            let session_live = self
                .runtime_state()
                .sessions
                .get(existing_session_id)
                .map(|session| {
                    session.status.is_live() && matches!(session.session_kind, SessionKind::Ssh)
                })
                .unwrap_or(false)
                && self.get_session(existing_session_id).is_ok();
            if session_live && !force_new_session {
                if activate_tab {
                    let _ = app_state.select_tab(&tab.id);
                    self.set_active_session(existing_session_id.to_string());
                }
                return Ok(existing_session_id.to_string());
            }
            // The operation worker closes this exact prior owner before it
            // admits the replacement. It must never be forgotten here while
            // exact teardown is still pending or retryable.
        }

        let session_id = next_ssh_session_id(&connection_id);
        let (key_file, key_error) = match self.materialize_ssh_key(&connection) {
            Ok(path) => (path, None),
            Err(error) => (None, Some(error)),
        };
        let launch = build_ssh_launch_spec(app_state, &tab, &connection, key_file.as_deref());

        let _ = app_state.update_ssh_tab_session(&tab.id, Some(session_id.clone()));
        if activate_tab {
            let _ = app_state.select_tab(&tab.id);
        }

        self.ensure_runtime_entry(&session_id, launch.cwd.clone(), dimensions);
        self.update_session_state(&session_id, |state| {
            state.status = SessionStatus::Starting;
            state.cwd = launch.cwd.clone();
            state.dimensions = dimensions;
            state.shell_program = launch.program.clone();
            state.configure_ssh(launch.clone());
            state.exit = None;
        });

        let existing_session_id = tab.pty_session_id.clone();
        if existing_session_id.is_some() {
            self.schedule_restart_ssh(
                existing_session_id,
                launch,
                session_id.clone(),
                dimensions,
                key_error,
                activate_tab,
                response,
            )?;
        } else {
            self.schedule_start_ssh(
                launch,
                session_id.clone(),
                dimensions,
                key_error,
                activate_tab,
                response,
            )?;
        }
        Ok(session_id)
    }

    pub fn restart_ssh_session(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<String, String> {
        self.restart_ssh_session_with_response(app_state, tab_id, dimensions, None)
    }

    pub fn restart_ssh_session_with_response(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<String, String> {
        let existing_session_id = app_state
            .find_ssh_tab(tab_id)
            .and_then(|tab| tab.pty_session_id.clone());

        let tab = app_state
            .find_ssh_tab(tab_id)
            .cloned()
            .ok_or_else(|| format!("Unknown SSH tab `{tab_id}`"))?;
        let connection_id = tab
            .ssh_connection_id
            .clone()
            .ok_or_else(|| format!("SSH tab `{tab_id}` is missing a connection id"))?;
        let connection = app_state
            .find_ssh_connection(&connection_id)
            .cloned()
            .ok_or_else(|| format!("Unknown SSH connection `{connection_id}`"))?;

        let session_id = next_ssh_session_id(&connection_id);
        let (key_file, key_error) = match self.materialize_ssh_key(&connection) {
            Ok(path) => (path, None),
            Err(error) => (None, Some(error)),
        };
        let launch = build_ssh_launch_spec(app_state, &tab, &connection, key_file.as_deref());

        let _ = app_state.update_ssh_tab_session(&tab.id, Some(session_id.clone()));
        let _ = app_state.select_tab(&tab.id);

        self.ensure_runtime_entry(&session_id, launch.cwd.clone(), dimensions);
        self.update_session_state(&session_id, |state| {
            state.status = SessionStatus::Starting;
            state.cwd = launch.cwd.clone();
            state.dimensions = dimensions;
            state.shell_program = launch.program.clone();
            state.configure_ssh(launch.clone());
            state.exit = None;
        });

        self.schedule_restart_ssh(
            existing_session_id,
            launch,
            session_id.clone(),
            dimensions,
            key_error,
            true,
            response,
        )?;
        Ok(session_id)
    }

    pub fn close_ssh_session(&self, app_state: &mut AppState, tab_id: &str) -> Result<(), String> {
        self.close_ssh_session_with_response(app_state, tab_id, None)
    }

    pub fn close_ssh_session_with_response(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let session_id = app_state
            .find_ssh_tab(tab_id)
            .and_then(|tab| tab.pty_session_id.clone());

        let _ = app_state.update_ssh_tab_session(tab_id, None);
        self.schedule_close_ssh(session_id, response)
    }

    pub fn reconcile_saved_ssh_tabs(&self, app_state: &mut AppState) -> usize {
        let runtime = self.runtime_state();
        let mut recovered = Vec::new();
        let existing_ids: std::collections::HashSet<String> = app_state
            .open_tabs
            .iter()
            .map(|tab| tab.id.clone())
            .collect();

        for session in runtime.sessions.values() {
            if !matches!(session.session_kind, SessionKind::Ssh) || !session.status.is_live() {
                continue;
            }

            let Some(tab_id) = session.tab_id.as_ref() else {
                continue;
            };
            if existing_ids.contains(tab_id) {
                continue;
            }

            let Some(connection_id) = session
                .ssh_launch
                .as_ref()
                .map(|launch| launch.ssh_connection_id.clone())
            else {
                continue;
            };
            let Some(connection) = app_state.find_ssh_connection(&connection_id) else {
                continue;
            };

            recovered.push(SessionTab {
                id: tab_id.clone(),
                tab_type: TabType::Ssh,
                project_id: session.project_id.clone().unwrap_or_default(),
                command_id: None,
                pty_session_id: Some(session.session_id.clone()),
                provider_session_id: None,
                label: Some(connection.label.clone()),
                ssh_connection_id: Some(connection_id),
                browser_workspace: None,
            });
        }

        app_state.merge_recovered_ssh_tabs(recovered)
    }

    pub fn restore_ssh_tabs(&self, app_state: &mut AppState) -> SshRestoreReport {
        let mut report = SshRestoreReport::default();
        let active_tab_id = app_state.active_tab_id.clone();

        let saved_ssh_tabs: Vec<String> = app_state.ssh_tabs().map(|tab| tab.id.clone()).collect();
        for tab_id in saved_ssh_tabs {
            let live_session_for_tab = self.runtime_state().sessions.values().find_map(|session| {
                (matches!(session.session_kind, SessionKind::Ssh)
                    && session.status.is_live()
                    && session.tab_id.as_deref() == Some(tab_id.as_str()))
                .then(|| session.session_id.clone())
            });
            if let Some(session_id) = live_session_for_tab {
                let _ = app_state.update_ssh_tab_session(&tab_id, Some(session_id));
                report.reattached += 1;
                continue;
            }

            let live_session = app_state
                .find_ssh_tab(&tab_id)
                .and_then(|tab| tab.pty_session_id.as_deref())
                .and_then(|session_id| self.runtime_state().sessions.get(session_id).cloned())
                .map(|session| {
                    session.status.is_live() && matches!(session.session_kind, SessionKind::Ssh)
                })
                .unwrap_or(false);

            if live_session {
                report.reattached += 1;
                continue;
            }

            let _ = app_state.update_ssh_tab_session(&tab_id, None);
            report.disconnected += 1;
        }

        report.recovered = self.reconcile_saved_ssh_tabs(app_state);
        let next_active = active_tab_id
            .filter(|tab_id| app_state.find_tab(tab_id).is_some())
            .or_else(|| app_state.open_tabs.first().map(|tab| tab.id.clone()));
        if app_state.active_tab_id != next_active {
            app_state.active_tab_id = next_active;
            app_state.mark_dirty();
        }

        report
    }

    pub fn start_server(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<(), String> {
        self.schedule_start_server(app_state, command_id, dimensions, true, None)
    }

    pub fn start_server_in_background(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<(), String> {
        self.schedule_start_server(app_state, command_id, dimensions, false, None)
    }

    pub fn start_server_with_remote_response(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        self.schedule_start_server(app_state, command_id, dimensions, activate_tab, response)
    }

    pub fn stop_server(&self, command_id: &str) -> Result<(), String> {
        self.update_session_state(command_id, |state| {
            state.note_user_stop_request();
            state.status = SessionStatus::Stopping;
            state.exit = Some(SessionExitState {
                code: None,
                signal: None,
                closed_by_user: true,
                summary: "Server stop requested".to_string(),
            });
            state.mark_dirty();
        });
        self.close_session(command_id)
    }

    pub fn stop_server_and_wait(&self, command_id: &str, timeout: Duration) -> bool {
        let _ = self.stop_server(command_id);
        if self.wait_for_session_shutdown(command_id, timeout) {
            return true;
        }

        let retry_result = self.retry_exact_session_teardown(command_id);
        if self.wait_for_session_shutdown(command_id, Duration::from_secs(2)) {
            return true;
        }

        if retry_result.is_ok() && !self.session_attached(command_id) {
            mark_session_reaped(&self.inner, command_id);
            return true;
        } else {
            let retry_detail = retry_result
                .err()
                .map(|error| format!(" Exact teardown remains retryable: {error}"))
                .unwrap_or_default();
            self.update_session_state(command_id, |state| {
                state.status = SessionStatus::Failed;
                state.pid = None;
                state.resources = ResourceSnapshot {
                    metrics_unavailable: true,
                    metrics_status: ProcessMetricStatus::Failed,
                    metric_values: ResourceMetricValueState::Unavailable,
                    cpu_value_state: ResourceMetricValueState::Unavailable,
                    memory_value_state: ResourceMetricValueState::Unavailable,
                    process_count_value_state: ResourceMetricValueState::Unavailable,
                    metrics_error: Some("exact_teardown_incomplete".to_string()),
                    last_sample_at: Some(Instant::now()),
                    ..ResourceSnapshot::default()
                };
                state.reap_incomplete = true;
                state.exit = Some(SessionExitState {
                    code: None,
                    signal: None,
                    closed_by_user: true,
                    summary: format!(
                        "Exact managed teardown is incomplete and retained for retry.{retry_detail}"
                    ),
                });
                state.mark_dirty();
            });
        }
        false
    }

    pub fn restart_server(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<(), String> {
        self.restart_server_with_banner(app_state, command_id, dimensions, "--- Restarting... ---")
    }

    pub fn restart_server_with_banner(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        banner: &str,
    ) -> Result<(), String> {
        self.schedule_restart_server(app_state, command_id, dimensions, banner, None)
    }

    pub fn restart_server_with_remote_response(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        banner: &str,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        self.schedule_restart_server(app_state, command_id, dimensions, banner, response)
    }

    pub fn start_all_for_project(
        &self,
        app_state: &mut AppState,
        project: &Project,
        dimensions: SessionDimensions,
    ) {
        for folder in &project.folders {
            for command in &folder.commands {
                let _ = self.start_server_in_background(app_state, &command.id, dimensions);
            }
        }
    }

    pub fn stop_all_for_project(&self, project_id: &str) {
        let command_ids: Vec<String> = self
            .runtime_state()
            .sessions
            .values()
            .filter(|session| {
                session.project_id.as_deref() == Some(project_id)
                    && session.command_id.is_some()
                    && matches!(
                        session.status,
                        SessionStatus::Running | SessionStatus::Starting
                    )
            })
            .filter_map(|session| session.command_id.clone())
            .take(MAX_PROCESS_OP_BATCH_ITEMS)
            .collect();
        for command_id in &command_ids {
            self.update_session_state(command_id, |state| {
                state.note_user_stop_request();
                state.status = SessionStatus::Stopping;
                state.mark_dirty();
            });
            let _ = self.enqueue_stop_server_and_wait(command_id, Duration::ZERO, None);
        }
    }

    pub fn stop_all_servers(&self) -> usize {
        let count = self
            .runtime_state()
            .sessions
            .values()
            .filter(|session| session.command_id.is_some() && session.status.is_live())
            .count();
        let _ = self.schedule_stop_all_servers(Duration::from_secs(5), None);
        count
    }

    pub fn live_session_count(&self) -> usize {
        self.runtime_state()
            .sessions
            .values()
            .filter(|session| session.status.is_live())
            .count()
    }

    pub fn close_all_live_sessions(&self) -> usize {
        let session_ids = self.live_session_ids();

        for session_id in &session_ids {
            let _ = self.close_session(session_id);
        }

        session_ids.len()
    }

    pub fn shutdown_managed_processes(&self, timeout: Duration) -> ManagedShutdownReport {
        let op_id = match self.schedule_shutdown(timeout) {
            Ok(op_id) => op_id,
            Err(_) => {
                return ManagedShutdownReport {
                    requested_sessions: self.live_session_count(),
                    ..ManagedShutdownReport::default()
                };
            }
        };
        let started = Instant::now();
        loop {
            for completion in self.drain_process_op_completions() {
                if completion.op_id == op_id {
                    if let Some(report) = completion.context.shutdown_report {
                        return report;
                    }
                    return ManagedShutdownReport::default();
                }
            }
            if started.elapsed() >= timeout + Duration::from_secs(2) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        ManagedShutdownReport {
            requested_sessions: self.live_session_count(),
            remaining_live_sessions: self.live_session_count(),
            remaining_tracked_pids: pid_file::active_tracked_pids().len(),
            ..ManagedShutdownReport::default()
        }
    }

    pub fn reconcile_saved_server_tabs(&self, app_state: &mut AppState) -> usize {
        let runtime = self.runtime_state();
        let mut recovered = Vec::new();
        let existing_ids: std::collections::HashSet<String> = app_state
            .open_tabs
            .iter()
            .map(|tab| tab.id.clone())
            .collect();

        for session in runtime.sessions.values() {
            let Some(command_id) = session.command_id.as_deref() else {
                continue;
            };
            if !matches!(
                session.status,
                SessionStatus::Running | SessionStatus::Starting
            ) {
                continue;
            }
            if existing_ids.contains(command_id) {
                continue;
            }
            if let Some(lookup) = app_state.find_command(command_id) {
                recovered.push(SessionTab {
                    id: command_id.to_string(),
                    tab_type: TabType::Server,
                    project_id: lookup.project.id.clone(),
                    command_id: Some(command_id.to_string()),
                    pty_session_id: Some(command_id.to_string()),
                    provider_session_id: None,
                    label: Some(lookup.command.label.clone()),
                    ssh_connection_id: None,
                    browser_workspace: None,
                });
            }
        }

        app_state.merge_recovered_server_tabs(recovered)
    }

    pub fn restore_saved_server_tabs(
        &self,
        app_state: &mut AppState,
        dimensions: SessionDimensions,
    ) -> usize {
        let active_tab_id = app_state.active_tab_id.clone();
        let command_ids: Vec<String> = app_state
            .open_tabs
            .iter()
            .filter(|tab| matches!(tab.tab_type, TabType::Server))
            .filter_map(|tab| tab.command_id.clone())
            .collect();

        let mut restored = 0;
        for command_id in command_ids {
            let already_live = self
                .runtime_state()
                .sessions
                .get(&command_id)
                .map(|session| session.status.is_live())
                .unwrap_or(false);
            if already_live {
                continue;
            }
            if self
                .start_server(app_state, &command_id, dimensions)
                .is_ok()
            {
                restored += 1;
            }
        }

        let next_active = active_tab_id
            .filter(|tab_id| app_state.find_tab(tab_id).is_some())
            .or_else(|| app_state.open_tabs.first().map(|tab| tab.id.clone()));
        if app_state.active_tab_id != next_active {
            app_state.active_tab_id = next_active;
            app_state.mark_dirty();
        }

        restored
    }

    fn session_exists(&self, session_id: &str) -> bool {
        let runtime_live = self
            .inner
            .runtime_state
            .read()
            .ok()
            .and_then(|runtime| {
                runtime
                    .sessions
                    .get(session_id)
                    .map(|session| session.status)
            })
            .map(SessionStatus::is_live)
            .unwrap_or(false);
        runtime_live
            && self
                .inner
                .sessions
                .lock()
                .ok()
                .map(|sessions| sessions.contains_key(session_id))
                .unwrap_or(false)
    }

    pub fn session_attached(&self, session_id: &str) -> bool {
        self.inner
            .sessions
            .lock()
            .ok()
            .map(|sessions| sessions.contains_key(session_id))
            .unwrap_or(false)
    }

    fn get_session(&self, session_id: &str) -> Result<Arc<TerminalSession>, String> {
        self.inner
            .sessions
            .lock()
            .map_err(|_| "Session store poisoned".to_string())?
            .get(session_id)
            .cloned()
            .ok_or_else(|| format!("Unknown session `{session_id}`"))
    }

    fn request_session_close(&self, session_id: &str, closed_by_user: bool) -> Result<(), String> {
        let closes_server = self
            .inner
            .runtime_state
            .read()
            .ok()
            .and_then(|runtime| {
                runtime
                    .sessions
                    .get(session_id)
                    .map(|session| session.session_kind)
            })
            .is_some_and(|kind| matches!(kind, SessionKind::Server));
        if closes_server {
            // Direct teardown is also a lifecycle fence for refresh callbacks
            // that did not pass through the process-op queue.
            bump_server_lifecycle_generation(&self.inner);
        }
        match close_exact_session_owner(&self.inner, session_id, closed_by_user) {
            Ok(true) => {
                // The exact Job/registry release and actor joins completed,
                // and the manager-owned TerminalSession was removed and
                // dropped before runtime/remote reconciliation.
                self.reconcile_closed_session(session_id);
                Ok(())
            }
            Ok(false) if session_projection_is_already_settled(&self.inner, session_id) => Ok(()),
            Ok(false) => Err(format!("Unknown session `{session_id}`")),
            Err(error) => {
                self.note_exact_teardown_failure(session_id, &error, closed_by_user);
                Err(error)
            }
        }
    }

    fn note_exact_teardown_failure(&self, session_id: &str, error: &str, closed_by_user: bool) {
        self.update_session_state(session_id, |session| {
            session.status = SessionStatus::Failed;
            session.reap_incomplete = true;
            session.exit = Some(SessionExitState {
                code: None,
                signal: None,
                closed_by_user,
                summary: format!("Exact managed teardown remains retryable: {error}"),
            });
            session.mark_dirty();
        });
    }

    fn live_session_ids(&self) -> Vec<String> {
        self.runtime_state()
            .sessions
            .values()
            .filter(|session| session.status.is_live())
            .map(|session| session.session_id.clone())
            .take(MAX_PROCESS_OP_BATCH_ITEMS)
            .collect()
    }

    fn wait_for_session_shutdown(&self, session_id: &str, timeout: Duration) -> bool {
        let started = Instant::now();
        loop {
            let session_settled = self
                .runtime_state()
                .sessions
                .get(session_id)
                .map(|session| session.status == SessionStatus::Stopped && !session.reap_incomplete)
                .unwrap_or(true);
            let tracked_pids = pid_file::active_tracked_pids_for_session(session_id);
            if session_settled && tracked_pids.is_empty() && !self.session_attached(session_id) {
                return true;
            }
            if started.elapsed() >= timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn retry_exact_session_teardown(&self, session_id: &str) -> Result<(), String> {
        retry_exact_session_teardown(&self.inner, session_id)
    }

    fn reconcile_closed_session(&self, session_id: &str) {
        if self.session_attached(session_id) {
            return;
        }
        // Exact close has already proved receiver-owned ACTIVE_PROCESS_ZERO,
        // joined the PTY actors, released the exact registry/Job entry, and
        // durably removed the matching ledger observation. PID scans are not
        // authority and must not delay or redirect this publication.
        let _ = pid_file::prune_inactive_entries();
        mark_session_reaped(&self.inner, session_id);
    }

    fn note_reap_incomplete(&self, session_id: &str) {
        self.update_session_state(session_id, |state| {
            state.reap_incomplete = true;
            state.status = SessionStatus::Failed;
            state.pid = None;
            state.resources = ResourceSnapshot {
                metrics_unavailable: true,
                metrics_status: ProcessMetricStatus::Failed,
                metric_values: ResourceMetricValueState::Unavailable,
                cpu_value_state: ResourceMetricValueState::Unavailable,
                memory_value_state: ResourceMetricValueState::Unavailable,
                process_count_value_state: ResourceMetricValueState::Unavailable,
                metrics_error: Some("exact_teardown_incomplete".to_string()),
                last_sample_at: Some(Instant::now()),
                ..ResourceSnapshot::default()
            };
            state.exit = Some(SessionExitState {
                code: None,
                signal: None,
                closed_by_user: state
                    .exit
                    .as_ref()
                    .map(|exit| exit.closed_by_user)
                    .unwrap_or(true),
                summary: "Exact managed teardown is incomplete and retained for retry.".to_string(),
            });
            state.mark_dirty();
        });
    }

    #[cfg(test)]
    fn ensure_session_replacement_safe_for_test(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> bool {
        ensure_prior_session_teardown_settled(&self.inner, session_id, timeout).is_ok()
    }

    fn update_session_state(&self, session_id: &str, f: impl FnOnce(&mut SessionRuntimeState)) {
        let mut runtime_changed = false;
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            if let Some(session) = runtime.sessions.get_mut(session_id) {
                let dirty_before = session.dirty_generation;
                f(session);
                runtime_changed = session.dirty_generation != dirty_before;
            }
        }
        if runtime_changed {
            bump_runtime_revision(&self.inner);
            mark_remote_session_dirty(&self.inner, session_id);
            emit_tracked_remote_runtime_snapshot(&self.inner, session_id);
        }
    }

    fn finalize_settled_session(&self, session_id: &str) -> Result<(), String> {
        if self.session_attached(session_id)
            || !session_projection_is_already_settled(&self.inner, session_id)
        {
            return Err(format!(
                "Session `{session_id}` cannot be forgotten before exact teardown settlement"
            ));
        }
        self.cleanup_ai_adapters_for_session(session_id);
        mark_remote_session_dirty(&self.inner, session_id);
        emit_remote_session_removed(&self.inner, session_id);
        Ok(())
    }

    fn ensure_runtime_entry(&self, session_id: &str, cwd: PathBuf, dimensions: SessionDimensions) {
        let mut inserted = false;
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            runtime
                .sessions
                .entry(session_id.to_string())
                .or_insert_with(|| {
                    inserted = true;
                    SessionRuntimeState::new(
                        session_id.to_string(),
                        cwd,
                        dimensions,
                        self.inner.terminal_backend,
                    )
                });
        }
        if inserted {
            bump_runtime_revision(&self.inner);
            mark_remote_session_dirty(&self.inner, session_id);
            emit_tracked_remote_runtime_snapshot(&self.inner, session_id);
        }
    }

    fn restore_active_session(&self, active_session_id: Option<String>) {
        let mut changed = false;
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            if runtime.active_session_id != active_session_id {
                runtime.active_session_id = active_session_id;
                changed = true;
            }
        }
        if changed {
            bump_runtime_revision(&self.inner);
        }
    }

    fn materialize_ssh_key(&self, connection: &SSHConnection) -> Result<Option<PathBuf>, String> {
        let dir = crate::persistence::app_config_dir()
            .map_err(|error| format!("resolve config dir: {error}"))?
            .join("ssh-keys");
        materialize_ssh_key_in(&dir, connection)
    }

    /// Best-effort cleanup when a connection is deleted or its key cleared.
    /// Materialized files are permission-locked, so a missed delete is low risk.
    pub fn remove_materialized_ssh_key(connection_id: &str) {
        let Ok(dir) = crate::persistence::app_config_dir() else {
            return;
        };
        let _ = std::fs::remove_file(dir.join("ssh-keys").join(safe_key_file_name(connection_id)));
    }
}

fn coordinate_user_origin_write(
    broker: &BrowserAttachmentBroker,
    session_id: &str,
    input: BrowserPromptInput<'_>,
    write: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    if !browser_input_opens_prompt_boundary(input) {
        return write("");
    }

    let reservation = broker.reserve_for_input(session_id, input);
    let prefix = reservation
        .as_ref()
        .map(|reservation| reservation.preamble())
        .unwrap_or_default();
    if let Err(error) = write(prefix) {
        if let Some(reservation) = reservation {
            let _ = broker.rollback(reservation);
        }
        return Err(error);
    }
    if let Some(reservation) = reservation {
        broker
            .commit(reservation)
            .map(|_| ())
            .map_err(|error| format!("commit browser attachments: {error}"))?;
    }
    Ok(())
}

fn unbind_attachment_if_matches(
    inner: &ProcessManagerInner,
    binding: Option<&BrowserAttachmentSessionBinding>,
) -> bool {
    binding.is_some_and(|binding| inner.browser_attachment_broker.unbind_if_matches(binding))
}

fn drain_claude_hook_sessions_inner(inner: &ProcessManagerInner) {
    let sessions = {
        let sessions = inner
            .claude_hook_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions
            .iter()
            .map(|(session_id, session)| (session_id.clone(), session.registration.clone()))
            .collect::<Vec<_>>()
    };
    for (session_id, registration) in sessions {
        fence_and_remove_claude_hook_session(inner, &session_id, Some(&registration));
    }
}

fn drain_browser_provider_sessions_inner(inner: &ProcessManagerInner) {
    let sessions = {
        let mut sessions = inner
            .browser_provider_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *sessions)
    };
    for (_, session) in sessions {
        session.registrar.revoke(&session.registration);
    }
}

impl Drop for ProcessManagerInner {
    fn drop(&mut self) {
        shutdown_process_manager_workers(self);
        drain_claude_hook_sessions_inner(self);
        drain_browser_provider_sessions_inner(self);
        remove_owned_claude_overlay_root(&self.claude_hook_temp_root);
    }
}

fn shutdown_process_manager_workers(inner: &ProcessManagerInner) {
    inner.background_stop.store(true, Ordering::SeqCst);
    let queue = inner.op_queue.lock().ok().and_then(|queue| queue.upgrade());
    if let Some(queue) = queue {
        queue.shutdown();
    }
    if let Ok(mut handle) = inner.background_thread.lock() {
        if let Some(handle) = handle.take() {
            handle.thread().unpark();
            join_process_manager_helper(handle);
        }
    }
    let workers = inner
        .auto_restart_workers
        .lock()
        .map(|mut workers| std::mem::take(&mut *workers))
        .unwrap_or_else(|_| std::process::abort());
    for worker in workers {
        join_process_manager_helper(worker);
    }

    // Worker admission is now closed and every helper has joined. Snapshot
    // the one real terminal objects without holding the store lock, then route
    // every remaining process tree through its exact coordinator authority.
    loop {
        let entry = {
            let sessions = inner
                .sessions
                .lock()
                .unwrap_or_else(|_| std::process::abort());
            let Some(session_id) = sessions.keys().next().cloned() else {
                break;
            };
            sessions
                .get(&session_id)
                .cloned()
                .map(|session| (session_id, session))
        };
        if let Some((session_id, session)) = entry {
            if let Err(error) = session.close(false) {
                eprintln!("process-manager shutdown failed exact terminal close: {error}");
                std::process::abort();
            }
            let mut sessions = inner
                .sessions
                .lock()
                .unwrap_or_else(|_| std::process::abort());
            if !sessions
                .get(&session_id)
                .is_some_and(|current| Arc::ptr_eq(current, &session))
            {
                std::process::abort();
            }
            sessions.remove(&session_id);
        }
    }
}

/// The managed-shutdown operation runs on the process-operation worker, so it
/// cannot invoke the full queue shutdown path without attempting to join
/// itself. Its admission fence is already published by `ProcessOpQueue::submit`;
/// stop and join the background/restart workers here before closing sessions so
/// no auto-restart can race the exact terminal teardown.
fn stop_background_workers_for_managed_shutdown(inner: &ProcessManagerInner) {
    inner.background_stop.store(true, Ordering::SeqCst);
    if let Ok(mut handle) = inner.background_thread.lock() {
        if let Some(handle) = handle.take() {
            handle.thread().unpark();
            join_process_manager_helper(handle);
        }
    }
    let workers = inner
        .auto_restart_workers
        .lock()
        .map(|mut workers| std::mem::take(&mut *workers))
        .unwrap_or_else(|_| std::process::abort());
    for worker in workers {
        join_process_manager_helper(worker);
    }
}

fn join_process_manager_helper(handle: thread::JoinHandle<()>) {
    if handle.thread().id() == thread::current().id() {
        std::process::abort();
    }
    let deadline = Instant::now()
        .checked_add(PROCESS_MANAGER_HELPER_JOIN_BUDGET)
        .unwrap_or_else(Instant::now);
    while !handle.is_finished() && Instant::now() < deadline {
        thread::yield_now();
    }
    if !handle.is_finished() {
        // A native helper that ignores its stop fence cannot be detached: it
        // may otherwise admit or mutate process state after shutdown returns.
        std::process::abort();
    }
    let _ = handle.join();
}

fn debug_enabled() -> bool {
    std::env::var("DEVMANAGER_TERMINAL_DEBUG")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn spawn_background_tasks(inner: Weak<ProcessManagerInner>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut system = sysinfo::System::new();
        loop {
            let Some(inner) = inner.upgrade() else {
                break;
            };
            if inner.background_stop.load(Ordering::SeqCst) {
                break;
            }

            #[cfg(test)]
            if let Some(hook) = inner
                .background_test_hook
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
            {
                hook();
            }
            if inner.background_stop.load(Ordering::SeqCst) {
                break;
            }

            refresh_resource_snapshots(&inner, &mut system);
            reconcile_ai_activity(&inner);
            reconcile_provider_terminal_exits(&inner);
            handle_auto_restart(&inner);
            reconcile_exit_states(&inner);

            drop(inner);

            thread::park_timeout(Duration::from_secs(1));
        }
    })
}

fn refresh_resource_snapshots(inner: &ProcessManagerInner, system: &mut sysinfo::System) {
    refresh_resource_snapshots_with_source(inner, system, None);
}

fn sampling_mutex_until<'a, T>(
    mutex: &'a Mutex<T>,
    absolute_deadline: Instant,
) -> Result<MutexGuard<'a, T>, ()> {
    loop {
        let remaining = absolute_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(());
        }
        match mutex.try_lock() {
            Ok(guard) => {
                if Instant::now() >= absolute_deadline {
                    return Err(());
                }
                return Ok(guard);
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                thread::sleep(remaining.min(Duration::from_millis(1)))
            }
            Err(std::sync::TryLockError::Poisoned(_)) => return Err(()),
        }
    }
}

fn sampling_read_until<'a, T>(
    lock: &'a RwLock<T>,
    absolute_deadline: Instant,
) -> Result<RwLockReadGuard<'a, T>, ()> {
    loop {
        let remaining = absolute_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(());
        }
        match lock.try_read() {
            Ok(guard) => {
                if Instant::now() >= absolute_deadline {
                    return Err(());
                }
                return Ok(guard);
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                thread::sleep(remaining.min(Duration::from_millis(1)))
            }
            Err(std::sync::TryLockError::Poisoned(_)) => return Err(()),
        }
    }
}

fn sampling_write_until<'a, T>(
    lock: &'a RwLock<T>,
    absolute_deadline: Instant,
) -> Result<RwLockWriteGuard<'a, T>, ()> {
    loop {
        let remaining = absolute_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(());
        }
        match lock.try_write() {
            Ok(guard) => {
                if Instant::now() >= absolute_deadline {
                    return Err(());
                }
                return Ok(guard);
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                thread::sleep(remaining.min(Duration::from_millis(1)))
            }
            Err(std::sync::TryLockError::Poisoned(_)) => return Err(()),
        }
    }
}

fn refresh_resource_snapshots_with_source(
    inner: &ProcessManagerInner,
    system: &mut sysinfo::System,
    source: Option<&ResourceSamplingSource>,
) {
    // The production deadline starts before any runtime/session enumeration.
    // Accounting never reads the legacy PID ledger: only the current
    // teardown-owned Job can grant membership.
    let sampled_at = Instant::now();
    let mut tick_budget = SamplingBudget::new(
        sampled_at + RESOURCE_SAMPLE_TICK_BUDGET,
        RESOURCE_SAMPLE_MAX_MEMBERS_PER_TICK,
    );
    let runtime = match sampling_read_until(&inner.runtime_state, tick_budget.deadline()) {
        Ok(runtime) => runtime,
        Err(()) => return,
    };
    let mut sessions: Vec<(
        String,
        u32,
        bool,
        SessionKind,
        SessionStatus,
        ResourceSnapshot,
    )> = Vec::new();
    for (id, session) in &runtime.sessions {
        if tick_budget.work_counters().runtime_sessions >= RESOURCE_SAMPLE_MAX_MEMBERS_PER_TICK
            || tick_budget.checkpoint().is_err()
        {
            break;
        }
        tick_budget.note_runtime_session();
        let (pid, status) = if session.status.is_live() {
            (session.pid, session.status)
        } else if session.reap_incomplete {
            (
                session.resources.process_ids.first().copied(),
                SessionStatus::Failed,
            )
        } else {
            continue;
        };
        if let Some(pid) = pid {
            sessions.push((
                id.clone(),
                pid,
                session.session_kind.is_ai(),
                session.session_kind,
                status,
                bounded_previous_snapshot(&session.resources, &mut tick_budget),
            ));
        }
    }
    drop(runtime);

    if sessions.is_empty() {
        if let Ok(mut samplers) =
            sampling_mutex_until(&inner.resource_samplers, tick_budget.deadline())
        {
            samplers.clear();
        }
        return;
    }

    // Snapshot TerminalSession Arcs without holding the sessions lock across OS queries.
    let mut terminal_sessions = HashMap::with_capacity(sessions.len());
    let guard = match sampling_mutex_until(&inner.sessions, tick_budget.deadline()) {
        Ok(guard) => guard,
        Err(()) => return,
    };
    for (session_id, _, _, _, _, _) in &sessions {
        if tick_budget.checkpoint().is_err() {
            break;
        }
        if let Some(session) = guard.get(session_id) {
            terminal_sessions.insert(session_id.clone(), session.clone());
        }
    }
    drop(guard);

    let mut job_member_observations: HashMap<String, ManagedJobObservationSnapshot> =
        HashMap::new();
    for (session_id, _, _, _, _, _) in &sessions {
        if tick_budget.checkpoint().is_err() {
            break;
        }
        tick_budget.note_session_authority_read();
        let observation = match job_query_member_limit(&tick_budget) {
            Err(error) => ManagedJobObservationSnapshot {
                capture: None,
                managed_process_fence: None,
                members: None,
                error: Some(fixed_sampler_error_code(&error).to_string()),
            },
            Ok(query_member_limit) => {
                match source.and_then(|source| source.sessions.get(session_id)) {
                    Some(source_session) => {
                        tick_budget.note_job_query();
                        match clone_injected_job_members_with_budget(
                            source_session,
                            query_member_limit,
                            &mut tick_budget,
                        ) {
                            Ok(members) => ManagedJobObservationSnapshot {
                                capture: None,
                                managed_process_fence: source_session.managed_process_fence.clone(),
                                members: Some(members),
                                error: None,
                            },
                            Err(error) => ManagedJobObservationSnapshot {
                                capture: None,
                                managed_process_fence: None,
                                members: None,
                                error: Some(fixed_sampler_error_code(&error).to_string()),
                            },
                        }
                    }
                    None => match terminal_sessions.get(session_id) {
                        Some(session) => {
                            tick_budget.note_job_query();
                            #[cfg(windows)]
                            let query = session.managed_process_observations_until(
                                tick_budget.deadline(),
                                query_member_limit,
                            );
                            #[cfg(not(windows))]
                            let query: Result<
                                Option<ManagedProcessObservationQuery>,
                                String,
                            > = Ok(None);
                            match query {
                                Ok(Some(query)) => {
                                    let (capture, members) = query.into_parts();
                                    match members {
                                        Ok(members) => match admit_job_observations_with_budget(
                                            &members,
                                            &mut tick_budget,
                                        ) {
                                            Ok(()) => ManagedJobObservationSnapshot {
                                                capture: Some(capture),
                                                managed_process_fence: None,
                                                members: Some(members),
                                                error: None,
                                            },
                                            Err(error) => ManagedJobObservationSnapshot {
                                                capture: Some(capture),
                                                managed_process_fence: None,
                                                members: None,
                                                error: Some(
                                                    fixed_sampler_error_code(&error).to_string(),
                                                ),
                                            },
                                        },
                                        Err(error) => ManagedJobObservationSnapshot {
                                            capture: Some(capture),
                                            managed_process_fence: None,
                                            members: None,
                                            error: Some(
                                                job_query_diagnostic_code(&error).to_string(),
                                            ),
                                        },
                                    }
                                }
                                Ok(None) => ManagedJobObservationSnapshot {
                                    capture: None,
                                    managed_process_fence: None,
                                    members: None,
                                    error: Some("job_authority_unavailable".to_string()),
                                },
                                Err(error) => ManagedJobObservationSnapshot {
                                    capture: None,
                                    managed_process_fence: None,
                                    members: None,
                                    error: Some(job_query_diagnostic_code(&error).to_string()),
                                },
                            }
                        }
                        None => ManagedJobObservationSnapshot {
                            capture: None,
                            managed_process_fence: None,
                            members: None,
                            error: Some("job_authority_unavailable".to_string()),
                        },
                    },
                }
            }
        };
        job_member_observations.insert(session_id.clone(), observation);
    }
    // Deduplicate all authoritative PIDs before building the one selected OS
    // metadata snapshot. Runtime roots never consume a slot unless the Job
    // itself reports that exact member.
    let mut process_ids = BTreeSet::new();
    'members: for observation in job_member_observations.values() {
        for member in observation.members().into_iter().flatten() {
            if tick_budget.checkpoint().is_err() {
                break 'members;
            }
            let pid = match member {
                JobMemberObservation::Accessible { identity } => identity.id().pid(),
                JobMemberObservation::Inaccessible { pid, .. } => *pid,
            };
            if process_ids.len() >= RESOURCE_SAMPLE_MAX_MEMBERS_PER_TICK
                && !process_ids.contains(&pid)
            {
                break 'members;
            }
            process_ids.insert(pid);
        }
    }
    let process_metadata = if let Some(source) = source {
        capture_injected_process_metadata(
            source,
            &job_member_observations,
            &process_ids,
            &mut tick_budget,
        )
    } else {
        capture_process_metadata(system, &process_ids, &mut tick_budget)
    };
    let logical_cpu_count = resolve_logical_cpu_count();
    let mut snapshots = Vec::with_capacity(sessions.len());
    let active_sampler_ids: BTreeSet<String> = sessions
        .iter()
        .map(|(session_id, _, _, _, _, _)| session_id.clone())
        .collect();
    let mut resource_samplers =
        match sampling_mutex_until(&inner.resource_samplers, tick_budget.deadline()) {
            Ok(samplers) => samplers,
            Err(()) => return,
        };
    resource_samplers.retain(|session_id, _| active_sampler_ids.contains(session_id));

    for (
        session_id,
        _runtime_pid,
        is_ai_session,
        resource_kind,
        lifecycle_status,
        previous_snapshot,
    ) in sessions
    {
        let tick_expired = tick_budget.checkpoint().is_err();
        if tick_budget.work_counters().projected_snapshots < RESOURCE_SAMPLE_MAX_MEMBERS_PER_TICK {
            tick_budget.note_projected_snapshot();
        }
        let job_observation = job_member_observations
            .remove(&session_id)
            .unwrap_or_else(|| ManagedJobObservationSnapshot {
                capture: None,
                managed_process_fence: None,
                members: None,
                error: Some("managed Job observation was not captured".to_string()),
            });
        let sample_ctx = ResourceSampleContext {
            is_ai_session,
            logical_cpu_count,
            sampled_at,
            resource_kind,
            lifecycle: process_lifecycle_from_status(lifecycle_status),
        };
        let sampled = if tick_expired {
            Some((
                stale_resource_snapshot(
                    system,
                    &session_id,
                    Some(&previous_snapshot),
                    sample_ctx,
                    Some("sampling_deadline_exceeded"),
                    &mut tick_budget,
                ),
                false,
            ))
        } else if let Some(job_members) = job_observation.members() {
            let sampler = resource_samplers
                .entry(session_id.clone())
                .or_insert_with(ProcessSampler::new);
            Some(sample_job_resources(
                &session_id,
                job_members,
                source
                    .and_then(|source| source.sessions.get(&session_id))
                    .map(|session| session.member_observations.as_slice()),
                &process_metadata,
                sample_ctx,
                sampler,
                &mut tick_budget,
            ))
        } else {
            Some((
                stale_resource_snapshot(
                    system,
                    &session_id,
                    Some(&previous_snapshot),
                    sample_ctx,
                    job_observation.error.as_deref(),
                    &mut tick_budget,
                ),
                false,
            ))
        };
        let (mut snapshot, awaiting_external_editor) = sampled.unwrap_or_else(|| {
            (
                ResourceSnapshot {
                    logical_cpu_count,
                    metrics_unavailable: true,
                    metrics_status: ProcessMetricStatus::Unknown,
                    metric_values: ResourceMetricValueState::Unavailable,
                    cpu_value_state: ResourceMetricValueState::Unavailable,
                    memory_value_state: ResourceMetricValueState::Unavailable,
                    metrics_stale: false,
                    metrics_error: Some("job_authority_unavailable".to_string()),
                    last_sample_at: Some(sampled_at),
                    ..ResourceSnapshot::default()
                },
                false,
            )
        });
        if !snapshot.metrics_stale && snapshot.metrics_status != ProcessMetricStatus::Failed {
            snapshot.managed_process_fence = job_observation.fence().cloned();
        } else {
            snapshot.managed_process_fence = None;
        }
        snapshots.push((
            session_id.clone(),
            snapshot,
            awaiting_external_editor,
            terminal_sessions.get(&session_id).cloned(),
            job_observation.capture,
        ));
    }
    drop(resource_samplers);

    let mut touched_sessions = Vec::new();
    let mut cleared_reap_sessions = Vec::new();
    let mut direct_snapshots = Vec::new();
    for (session_id, snapshot, awaiting_external_editor, terminal_session, capture) in snapshots {
        #[cfg(windows)]
        if source.is_none() {
            let publication = match (terminal_session.as_ref(), capture.as_ref()) {
                (Some(session), Some(capture)) => session
                    .publish_managed_resource_sample_if_current(
                        capture,
                        snapshot,
                        awaiting_external_editor,
                        tick_budget.deadline(),
                    ),
                _ => Err("managed sampling authority unavailable".to_string()),
            };
            match publication {
                Ok(ManagedResourceSamplePublication::Published {
                    dirty_changed,
                    cleared_unreaped,
                }) => {
                    if cleared_unreaped {
                        cleared_reap_sessions.push(session_id.clone());
                    }
                    if dirty_changed {
                        touched_sessions.push(session_id);
                    }
                }
                Ok(ManagedResourceSamplePublication::StaleGeneration { dirty_changed }) => {
                    if let Ok(mut samplers) =
                        sampling_mutex_until(&inner.resource_samplers, tick_budget.deadline())
                    {
                        samplers.remove(&session_id);
                    }
                    if dirty_changed {
                        touched_sessions.push(session_id);
                    }
                }
                Err(_) => {
                    if let Ok(mut samplers) =
                        sampling_mutex_until(&inner.resource_samplers, tick_budget.deadline())
                    {
                        samplers.remove(&session_id);
                    }
                }
            }
            continue;
        }

        direct_snapshots.push((session_id, snapshot, awaiting_external_editor));
    }

    #[cfg(test)]
    if let Some(delay) = source.and_then(|source| source.before_direct_publication_delay) {
        thread::sleep(delay);
    }

    if !direct_snapshots.is_empty() && tick_budget.checkpoint().is_ok() {
        if let Ok(mut runtime) = sampling_write_until(&inner.runtime_state, tick_budget.deadline())
        {
            for (session_id, snapshot, awaiting_external_editor) in direct_snapshots {
                if tick_budget.checkpoint().is_err() {
                    break;
                }
                let Some(session) = runtime.sessions.get_mut(&session_id) else {
                    continue;
                };
                if tick_budget.checkpoint().is_err() {
                    break;
                }
                let dirty_before = session.dirty_generation;
                let cleared_unreaped = session.reap_incomplete && snapshot.process_ids.is_empty();
                session.note_resource_sample(snapshot);
                session.note_external_editor_wait(awaiting_external_editor);
                if cleared_unreaped {
                    cleared_reap_sessions.push(session_id.clone());
                }
                if session.dirty_generation != dirty_before {
                    touched_sessions.push(session_id);
                }
            }
        }
    }
    drop(terminal_sessions);
    if !touched_sessions.is_empty() {
        bump_runtime_revision(inner);
    }
    for session_id in touched_sessions {
        emit_tracked_remote_runtime_snapshot(inner, &session_id);
    }
    for session_id in cleared_reap_sessions {
        let _ = pid_file::prune_inactive_entries();
        mark_session_reaped(inner, &session_id);
    }
}

fn job_query_member_limit(budget: &SamplingBudget) -> Result<usize, SamplerError> {
    budget.checkpoint()?;
    let remaining = budget.remaining_members();
    if remaining == 0 {
        return Err(SamplerError::WorkBudgetExceeded {
            attempted: budget.claimed_members().saturating_add(1),
            max: budget.max_members(),
        });
    }
    Ok(remaining)
}

#[derive(Debug, Clone, Default)]
struct ProcessProjectionMetadata {
    parent_pid: Option<u32>,
    display_name: String,
    command_label: String,
    command_arg_count: u16,
    command_arg_bytes: u32,
    blocking_external_editor: bool,
}

fn sample_job_resources(
    session_id: &str,
    job_members: &[JobMemberObservation],
    injected_member_observations: Option<&[ProcessMemberObservation]>,
    metadata: &HashMap<u32, ProcessProjectionMetadata>,
    ctx: ResourceSampleContext,
    sampler: &mut ProcessSampler,
    budget: &mut SamplingBudget,
) -> (ResourceSnapshot, bool) {
    let current_members = match injected_member_observations {
        Some(members) => {
            match clone_injected_member_observations_with_budget(job_members, members, budget) {
                Ok(members) => members,
                Err(error) => {
                    return (
                        budget_failed_resource_snapshot(session_id, job_members, ctx, error),
                        false,
                    );
                }
            }
        }
        None => match observe_job_members_with_budget(job_members, budget) {
            Ok(members) => members,
            Err(error) => {
                return (
                    budget_failed_resource_snapshot(session_id, job_members, ctx, error),
                    false,
                );
            }
        },
    };
    let owned_pids = unique_job_member_pids(job_members);
    let awaiting_external_editor = ctx.is_ai_session
        && owned_pids.iter().any(|pid| {
            metadata
                .get(pid)
                .is_some_and(|row| row.blocking_external_editor)
        });
    let snapshot = build_resource_snapshot(
        metadata,
        session_id,
        &owned_pids,
        ctx.logical_cpu_count,
        ctx.sampled_at,
        job_members,
        current_members.as_slice(),
        sampler,
        ctx,
        budget,
    );
    (snapshot, awaiting_external_editor)
}

fn stale_resource_snapshot(
    _system: &sysinfo::System,
    resource_id: &str,
    previous: Option<&ResourceSnapshot>,
    ctx: ResourceSampleContext,
    job_error: Option<&str>,
    budget: &mut SamplingBudget,
) -> ResourceSnapshot {
    let mut snapshot = previous
        .map(|previous| bounded_previous_snapshot(previous, budget))
        .unwrap_or_default();
    let mut safe_processes = Vec::with_capacity(snapshot.processes.len());
    for mut process in snapshot.processes {
        if budget.work_counters().projected_rows >= budget.max_members()
            || budget.checkpoint().is_err()
        {
            break;
        }
        budget.note_projected_row();
        let safe_label = classify_process_display_name(&process.name, &[]);
        process.name = format!("{safe_label} (metrics unavailable)");
        process.executable = process
            .executable
            .as_deref()
            .and_then(|value| redacted_executable_basename(Path::new(value)));
        process.command_label = Some(safe_label);
        process.resource_kind = sanitize_resource_kind(process.resource_kind.as_deref());
        process.resource_id = Some(sanitize_opaque_resource_id(
            process.resource_id.as_deref().unwrap_or(resource_id),
        ));
        // The Job query failed and cached members are not resampled. The
        // selected OS list therefore cannot distinguish a vanished process
        // from a process omitted because membership was unavailable; preserve
        // the safe Unknown state rather than copying session lifecycle.
        process.lifecycle = ProcessResourceLifecycle::Unknown;
        process.metrics_status = ProcessMetricStatus::Unknown;
        process.cpu_value_state = last_known_metric_state(process.cpu_value_state);
        process.memory_value_state = last_known_metric_state(process.memory_value_state);
        process.metric_values =
            combined_metric_state(process.cpu_value_state, process.memory_value_state);
        safe_processes.push(process);
    }
    snapshot.processes = safe_processes;
    snapshot.process_ids = snapshot
        .processes
        .iter()
        .map(|process| process.pid)
        .collect();
    // Retained aggregate confidence comes from the last aggregate sample,
    // not from whichever bounded display rows survived this stale projection.
    // The count is explicitly LastKnown below and carries no action fence.
    snapshot.process_count_value_state =
        last_known_metric_state(snapshot.process_count_value_state);
    snapshot.logical_cpu_count = ctx.logical_cpu_count.max(1);
    snapshot.metrics_unavailable = true;
    snapshot.metrics_status = ProcessMetricStatus::Unknown;
    snapshot.cpu_value_state = last_known_metric_state(snapshot.cpu_value_state);
    snapshot.memory_value_state = last_known_metric_state(snapshot.memory_value_state);
    snapshot.metric_values =
        combined_metric_state(snapshot.cpu_value_state, snapshot.memory_value_state);
    snapshot.metrics_stale = true;
    snapshot.metrics_error = Some(fixed_job_failure_code(job_error).to_string());
    snapshot.managed_process_fence = None;
    snapshot.last_sample_at = Some(ctx.sampled_at);
    snapshot
}

fn bounded_previous_snapshot(
    previous: &ResourceSnapshot,
    budget: &mut SamplingBudget,
) -> ResourceSnapshot {
    let mut process_ids = Vec::with_capacity(
        previous
            .process_ids
            .len()
            .min(RESOURCE_SAMPLE_MAX_MEMBERS_PER_TICK),
    );
    for pid in previous
        .process_ids
        .iter()
        .copied()
        .take(RESOURCE_SAMPLE_MAX_MEMBERS_PER_TICK)
    {
        if budget.work_counters().cached_process_ids >= budget.max_members()
            || budget.checkpoint().is_err()
        {
            break;
        }
        budget.note_cached_process_id();
        process_ids.push(pid);
    }
    let mut processes = Vec::with_capacity(
        previous
            .processes
            .len()
            .min(RESOURCE_SAMPLE_MAX_MEMBERS_PER_TICK),
    );
    for process in previous
        .processes
        .iter()
        .take(RESOURCE_SAMPLE_MAX_MEMBERS_PER_TICK)
    {
        if budget.work_counters().cached_process_rows >= budget.max_members()
            || budget.checkpoint().is_err()
        {
            break;
        }
        budget.note_cached_process_row();
        processes.push(process.clone());
    }
    ResourceSnapshot {
        cpu_percent: previous.cpu_percent,
        core_equivalent_percent: previous.core_equivalent_percent,
        memory_bytes: previous.memory_bytes,
        memory_metric: previous.memory_metric,
        process_count: previous
            .process_count
            .min(RESOURCE_SAMPLE_MAX_MEMBERS_PER_TICK as u32),
        process_count_value_state: previous.process_count_value_state,
        process_ids,
        metrics_unavailable: previous.metrics_unavailable,
        metrics_status: previous.metrics_status,
        metric_values: previous.metric_values,
        cpu_value_state: previous.cpu_value_state,
        memory_value_state: previous.memory_value_state,
        metrics_stale: previous.metrics_stale,
        // Diagnostics are reconstructed from fixed codes at the current
        // projection boundary; never clone arbitrary prior text.
        metrics_error: None,
        sampling_generation: previous.sampling_generation,
        io_read_bytes: previous.io_read_bytes,
        io_write_bytes: previous.io_write_bytes,
        processes,
        logical_cpu_count: previous.logical_cpu_count.max(1),
        managed_process_fence: None,
        last_sample_at: previous.last_sample_at,
    }
}

fn observe_job_members_with_budget(
    job_members: &[JobMemberObservation],
    budget: &mut SamplingBudget,
) -> Result<Vec<ProcessMemberObservation>, SamplerError> {
    let mut observations = Vec::with_capacity(job_members.len().min(budget.max_members()));
    for member in job_members.iter().take(budget.max_members()) {
        budget.checkpoint()?;
        budget.note_metric_observation();
        observations.push(job_member_to_process_observation(member));
        budget.checkpoint()?;
    }
    Ok(observations)
}

fn admit_job_observations_with_budget(
    job_members: &[JobMemberObservation],
    budget: &mut SamplingBudget,
) -> Result<(), SamplerError> {
    for member in job_members.iter().take(budget.max_members()) {
        budget.checkpoint()?;
        budget.note_job_candidate();
        budget.note_identity_inspection();
        match member {
            JobMemberObservation::Accessible { identity } => {
                budget.admit_identity(identity)?;
            }
            JobMemberObservation::Inaccessible {
                pid,
                creation_time_100ns,
                ..
            } => {
                budget.admit_inaccessible(*pid, *creation_time_100ns)?;
            }
        }
        budget.checkpoint()?;
    }
    Ok(())
}

fn clone_injected_job_members_with_budget(
    source: &ResourceSamplingSession,
    query_member_limit: usize,
    budget: &mut SamplingBudget,
) -> Result<Vec<JobMemberObservation>, SamplerError> {
    if source.job_members.len() > query_member_limit {
        return Err(SamplerError::WorkBudgetExceeded {
            attempted: budget
                .claimed_members()
                .saturating_add(source.job_members.len()),
            max: budget.max_members(),
        });
    }
    let fence =
        source
            .managed_process_fence
            .as_ref()
            .ok_or_else(|| SamplerError::ObservationFailed {
                pid: 0,
                reason: "injected_source_missing_exact_fence".to_string(),
            })?;
    let root_pid = fence.root().id().pid();
    let mut members = Vec::with_capacity(source.job_members.len().min(query_member_limit));
    let mut exact_root_observed = false;
    for member in &source.job_members {
        budget.checkpoint()?;
        budget.note_job_candidate();
        budget.note_identity_inspection();
        let safe_member = match member {
            JobMemberObservation::Accessible { identity } => {
                if identity.id().pid() == root_pid && identity != fence.root() {
                    return Err(SamplerError::ConflictingProcessIdentity { pid: root_pid });
                }
                exact_root_observed |= identity == fence.root();
                budget.admit_identity(identity)?;
                JobMemberObservation::Accessible {
                    identity: identity.clone(),
                }
            }
            JobMemberObservation::Inaccessible {
                pid,
                creation_time_100ns,
                ..
            } => {
                if *pid == root_pid {
                    return Err(SamplerError::ConflictingProcessIdentity { pid: root_pid });
                }
                budget.admit_inaccessible(*pid, *creation_time_100ns)?;
                JobMemberObservation::Inaccessible {
                    pid: *pid,
                    creation_time_100ns: *creation_time_100ns,
                    reason: "member_metrics_unavailable".to_string(),
                }
            }
        };
        members.push(safe_member);
        budget.checkpoint()?;
    }
    if !exact_root_observed {
        return Err(SamplerError::ObservationFailed {
            pid: root_pid,
            reason: "injected_source_missing_exact_root".to_string(),
        });
    }
    Ok(members)
}

fn clone_injected_member_observations_with_budget(
    job_members: &[JobMemberObservation],
    observations: &[ProcessMemberObservation],
    budget: &mut SamplingBudget,
) -> Result<Vec<ProcessMemberObservation>, SamplerError> {
    let authoritative_pids = unique_job_member_pids(job_members)
        .into_iter()
        .collect::<BTreeSet<_>>();
    if observations.len() > budget.max_members() || observations.len() != authoritative_pids.len() {
        return Err(SamplerError::WorkBudgetExceeded {
            attempted: observations.len(),
            max: authoritative_pids.len().min(budget.max_members()),
        });
    }

    let mut seen = BTreeSet::new();
    let mut safe = Vec::with_capacity(observations.len());
    for observation in observations {
        budget.checkpoint()?;
        budget.note_metric_observation();
        let pid = observation.pid();
        if !authoritative_pids.contains(&pid) || !seen.insert(pid) {
            return Err(SamplerError::ConflictingProcessIdentity { pid });
        }
        budget.admit_observation(observation)?;
        safe.push(match observation {
            ProcessMemberObservation::Accessible(member) => {
                ProcessMemberObservation::Accessible(member.clone())
            }
            ProcessMemberObservation::Inaccessible(member) => {
                ProcessMemberObservation::Inaccessible(
                    InaccessibleProcess::new(member.pid, member.creation_time_100ns)
                        .with_reason("member_metrics_unavailable"),
                )
            }
        });
        budget.checkpoint()?;
    }
    Ok(safe)
}

fn budget_failed_resource_snapshot(
    _resource_id: &str,
    job_members: &[JobMemberObservation],
    ctx: ResourceSampleContext,
    error: SamplerError,
) -> ResourceSnapshot {
    let process_ids = unique_job_member_pids(job_members)
        .into_iter()
        .take(RESOURCE_SAMPLE_MAX_MEMBERS_PER_TICK)
        .collect::<Vec<_>>();
    ResourceSnapshot {
        memory_metric: resource_memory_metric(),
        process_count: process_ids.len() as u32,
        process_count_value_state: ResourceMetricValueState::Observed,
        process_ids,
        metrics_unavailable: true,
        metrics_status: ProcessMetricStatus::Failed,
        metric_values: ResourceMetricValueState::Unavailable,
        cpu_value_state: ResourceMetricValueState::Unavailable,
        memory_value_state: ResourceMetricValueState::Unavailable,
        metrics_stale: false,
        metrics_error: Some(fixed_sampler_error_code(&error).to_string()),
        logical_cpu_count: ctx.logical_cpu_count.max(1),
        last_sample_at: Some(ctx.sampled_at),
        ..ResourceSnapshot::default()
    }
}

fn sanitize_opaque_resource_id(resource_id: &str) -> String {
    let is_opaque = resource_id.len() == "resource-".len() + 16
        && resource_id.starts_with("resource-")
        && resource_id["resource-".len()..]
            .chars()
            .all(|character| character.is_ascii_hexdigit());
    if is_opaque {
        resource_id.to_string()
    } else {
        opaque_resource_id(resource_id)
    }
}

#[derive(Clone, Copy)]
struct ResourceSampleContext {
    is_ai_session: bool,
    logical_cpu_count: u32,
    sampled_at: Instant,
    resource_kind: SessionKind,
    lifecycle: ProcessResourceLifecycle,
}

fn process_lifecycle_from_status(status: SessionStatus) -> ProcessResourceLifecycle {
    match status {
        SessionStatus::Starting => ProcessResourceLifecycle::Starting,
        SessionStatus::Running => ProcessResourceLifecycle::Running,
        SessionStatus::Stopping => ProcessResourceLifecycle::Stopping,
        SessionStatus::Stopped | SessionStatus::Exited => ProcessResourceLifecycle::Stopped,
        SessionStatus::Crashed | SessionStatus::Failed => ProcessResourceLifecycle::Failed,
    }
}

fn capture_process_metadata(
    system: &mut sysinfo::System,
    process_ids: &BTreeSet<u32>,
    budget: &mut SamplingBudget,
) -> HashMap<u32, ProcessProjectionMetadata> {
    budget.note_metadata_snapshot();
    if process_ids.is_empty() || budget.checkpoint().is_err() {
        return HashMap::new();
    }
    let pids: Vec<sysinfo::Pid> = process_ids
        .iter()
        .copied()
        .take(budget.max_members())
        .map(sysinfo::Pid::from_u32)
        .collect();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&pids),
        true,
        sysinfo::ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always),
    );
    if budget.checkpoint().is_err() {
        return HashMap::new();
    }

    let mut metadata = HashMap::with_capacity(pids.len());
    for pid in process_ids.iter().copied().take(budget.max_members()) {
        if budget.checkpoint().is_err() {
            break;
        }
        budget.note_metadata_row();
        let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) else {
            continue;
        };
        let os_name = process.name().to_string_lossy();
        let (command, command_arg_count, command_arg_bytes) = bounded_command_shape(process);
        let command_label = classify_process_display_name(&os_name, &command);
        let blocking_external_editor = is_blocking_external_editor_name(&os_name);
        metadata.insert(
            pid,
            ProcessProjectionMetadata {
                parent_pid: process.parent().map(|parent| parent.as_u32()),
                display_name: command_label.clone(),
                command_label,
                command_arg_count,
                command_arg_bytes,
                blocking_external_editor,
            },
        );
    }
    metadata
}

fn capture_injected_process_metadata(
    source: &ResourceSamplingSource,
    observations: &HashMap<String, ManagedJobObservationSnapshot>,
    authoritative_process_ids: &BTreeSet<u32>,
    budget: &mut SamplingBudget,
) -> HashMap<u32, ProcessProjectionMetadata> {
    budget.note_metadata_snapshot();
    if authoritative_process_ids.is_empty() || budget.checkpoint().is_err() {
        return HashMap::new();
    }
    let max_rows = authoritative_process_ids
        .len()
        .min(budget.claimed_members())
        .min(budget.max_members());
    let mut metadata = HashMap::with_capacity(max_rows);
    'sessions: for (session_id, observation) in observations {
        let Some(source_session) = source.sessions.get(session_id) else {
            continue;
        };
        let Some(members) = observation.members() else {
            continue;
        };
        for pid in unique_job_member_pids(members) {
            if metadata.len() >= max_rows || budget.checkpoint().is_err() {
                break 'sessions;
            }
            if !authoritative_process_ids.contains(&pid) || metadata.contains_key(&pid) {
                continue;
            }
            budget.note_metadata_row();
            let Some(row) = source_session.metadata.get(&pid) else {
                continue;
            };
            let display_input = bounded_injected_metadata_string(&row.display_name);
            let command_input = bounded_injected_metadata_string(&row.command_label);
            metadata.insert(
                pid,
                ProcessProjectionMetadata {
                    parent_pid: row
                        .parent_pid
                        .filter(|parent| authoritative_process_ids.contains(parent)),
                    display_name: allowlisted_process_label(&display_input),
                    command_label: allowlisted_process_label(&command_input),
                    command_arg_count: row.command_arg_count.min(MAX_COMMAND_ARGUMENTS as u16),
                    command_arg_bytes: row.command_arg_bytes.min(MAX_COMMAND_ARGUMENT_BYTES as u32),
                    blocking_external_editor: row.blocking_external_editor,
                },
            );
        }
    }
    metadata
}

fn bounded_injected_metadata_string(value: &str) -> String {
    const MAX_INPUT_BYTES: usize = 96;
    let mut bounded = String::with_capacity(value.len().min(MAX_INPUT_BYTES));
    for character in value.chars() {
        if bounded.len().saturating_add(character.len_utf8()) > MAX_INPUT_BYTES {
            break;
        }
        bounded.push(character);
    }
    bounded
}

fn build_resource_snapshot(
    metadata: &HashMap<u32, ProcessProjectionMetadata>,
    resource_id: &str,
    owned_pids: &[u32],
    logical_cpu_count: u32,
    sampled_at: Instant,
    authoritative_job_members: &[JobMemberObservation],
    member_observations: &[ProcessMemberObservation],
    sampler: &mut ProcessSampler,
    ctx: ResourceSampleContext,
    budget: &mut SamplingBudget,
) -> ResourceSnapshot {
    if budget.checkpoint().is_err() {
        return ResourceSnapshot {
            logical_cpu_count: logical_cpu_count.max(1),
            metrics_unavailable: true,
            metrics_status: ProcessMetricStatus::Failed,
            metric_values: ResourceMetricValueState::Unavailable,
            cpu_value_state: ResourceMetricValueState::Unavailable,
            memory_value_state: ResourceMetricValueState::Unavailable,
            metrics_error: Some("sampling_deadline_exceeded".to_string()),
            last_sample_at: Some(sampled_at),
            ..ResourceSnapshot::default()
        };
    }
    let observations = member_observations
        .iter()
        .take(budget.max_members())
        .cloned()
        .collect::<Vec<_>>();
    let accounting_result = sampler.sample_now_with_budget(logical_cpu_count, observations, budget);
    let accounting = accounting_result.as_ref().ok().cloned();
    let accounting_error = accounting_result
        .as_ref()
        .err()
        .map(fixed_sampler_error_code)
        .map(str::to_string);
    let accounting_diagnostic = accounting
        .as_ref()
        .and_then(|snapshot| snapshot.error.as_deref())
        .map(|_| "member_metrics_partial".to_string());
    let member_by_pid: HashMap<u32, &ProcessAccountingMemberSnapshot> = accounting
        .as_ref()
        .map(|snapshot| {
            snapshot
                .members
                .iter()
                .map(|member| (member.pid, member))
                .collect()
        })
        .unwrap_or_default();
    let job_member_by_pid: HashMap<u32, &JobMemberObservation> = authoritative_job_members
        .iter()
        .map(|member| {
            let pid = match member {
                JobMemberObservation::Accessible { identity } => identity.id().pid(),
                JobMemberObservation::Inaccessible { pid, .. } => *pid,
            };
            (pid, member)
        })
        .collect();
    let mut processes = Vec::with_capacity(owned_pids.len().min(budget.max_members()));

    for pid in owned_pids.iter().take(budget.max_members()) {
        if budget.work_counters().projected_rows >= budget.max_members()
            || budget.checkpoint().is_err()
        {
            break;
        }
        budget.note_projected_row();
        let metadata = metadata.get(pid);
        let member = member_by_pid.get(pid).copied();
        let job_member = job_member_by_pid.get(pid).copied();
        let process_cpu = member
            .and_then(|member| member.machine_cpu_percent)
            .unwrap_or(0.0) as f32;
        let process_memory = member
            .and_then(|member| member.private_memory_bytes)
            .unwrap_or(0);
        let name = metadata
            .map(|metadata| metadata.display_name.clone())
            .or_else(|| match job_member {
                Some(JobMemberObservation::Accessible { identity }) => {
                    Some(allowlisted_process_label(
                        identity
                            .canonical_executable()
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("unknown"),
                    ))
                }
                _ => None,
            })
            .unwrap_or_else(|| "Other process".to_string());
        let name = if member.is_some_and(|member| member.metrics_unavailable) {
            format!("{name} (metrics unavailable)")
        } else {
            name
        };
        let metrics_status = member
            .map(|member| member.status)
            .or_else(|| {
                accounting_error
                    .as_ref()
                    .map(|_| ProcessMetricStatus::Failed)
            })
            .unwrap_or(ProcessMetricStatus::Unknown);
        let exact_executable = member
            .and_then(|member| member.executable.clone())
            .or_else(|| {
                member_observations.iter().find_map(|member| match member {
                    ProcessMemberObservation::Accessible(member)
                        if member.identity.id().pid() == *pid =>
                    {
                        redacted_executable_basename(member.identity.canonical_executable())
                    }
                    _ => None,
                })
            })
            .or_else(|| match job_member {
                Some(JobMemberObservation::Accessible { identity }) => {
                    redacted_executable_basename(identity.canonical_executable())
                }
                _ => None,
            });
        let creation_time_100ns = member
            .and_then(|member| member.creation_time_100ns)
            .or_else(|| match job_member {
                Some(JobMemberObservation::Accessible { identity }) => {
                    Some(identity.id().creation_time_100ns())
                }
                Some(JobMemberObservation::Inaccessible {
                    creation_time_100ns,
                    ..
                }) => *creation_time_100ns,
                None => None,
            });
        let cpu_value_state = if member
            .and_then(|member| member.machine_cpu_percent)
            .is_some()
        {
            ResourceMetricValueState::Observed
        } else {
            ResourceMetricValueState::Unavailable
        };
        let memory_value_state = if member
            .and_then(|member| member.private_memory_bytes)
            .is_some()
        {
            ResourceMetricValueState::Observed
        } else {
            ResourceMetricValueState::Unavailable
        };
        processes.push(crate::state::ProcessResourceNode {
            pid: *pid,
            parent_pid: metadata.and_then(|metadata| metadata.parent_pid),
            name,
            cpu_percent: process_cpu,
            core_equivalent_percent: member
                .and_then(|member| member.core_equivalent_percent)
                .unwrap_or(0.0) as f32,
            memory_bytes: process_memory,
            memory_metric: resource_memory_metric(),
            creation_time_100ns,
            executable: exact_executable,
            command_label: Some(
                metadata
                    .map(|metadata| metadata.command_label.clone())
                    .unwrap_or_else(|| "Other process".to_string()),
            ),
            command_arg_count: metadata
                .map(|metadata| metadata.command_arg_count)
                .unwrap_or_default(),
            command_arg_bytes: metadata
                .map(|metadata| metadata.command_arg_bytes)
                .unwrap_or_default(),
            resource_id: Some(opaque_resource_id(resource_id)),
            resource_kind: Some(resource_kind_label(ctx.resource_kind).to_string()),
            child_count: 0,
            lifecycle: process_resource_lifecycle(ctx.lifecycle, job_member),
            metrics_status,
            metric_values: combined_metric_state(cpu_value_state, memory_value_state),
            cpu_value_state,
            memory_value_state,
            sampling_generation: member
                .map(|member| member.generation)
                .or_else(|| accounting.as_ref().map(|snapshot| snapshot.generation))
                .unwrap_or_default(),
        });
    }

    // Parent links are attribution metadata only; ownership remains the Job
    // member set. Compute child counts from the same bounded projection.
    let parent_counts = processes
        .iter()
        .fold(HashMap::<u32, u32>::new(), |mut counts, node| {
            if let Some(parent_pid) = node.parent_pid {
                let entry = counts.entry(parent_pid).or_default();
                *entry = entry.saturating_add(1);
            }
            counts
        });
    for process in &mut processes {
        process.child_count = parent_counts.get(&process.pid).copied().unwrap_or(0);
    }

    let process_ids = accounting
        .as_ref()
        .map(|snapshot| snapshot.members.iter().map(|member| member.pid).collect())
        .unwrap_or_else(|| {
            processes
                .iter()
                .map(|process| process.pid)
                .collect::<Vec<_>>()
        });
    let (cpu_percent, core_equivalent_percent, memory_bytes, process_count) = accounting
        .as_ref()
        .map(|snapshot| {
            (
                snapshot.machine_cpu_percent as f32,
                snapshot.core_equivalent_percent as f32,
                snapshot.memory_bytes,
                snapshot.process_count,
            )
        })
        .unwrap_or((0.0, 0.0, 0, process_ids.len() as u32));
    let cpu_value_state = accounting
        .as_deref()
        .map(|snapshot| {
            current_metric_state(&snapshot.members, |member| {
                member.machine_cpu_percent.is_some()
            })
        })
        .unwrap_or(ResourceMetricValueState::Unavailable);
    let memory_value_state = accounting
        .as_deref()
        .map(|snapshot| {
            current_metric_state(&snapshot.members, |member| {
                member.private_memory_bytes.is_some()
            })
        })
        .unwrap_or(ResourceMetricValueState::Unavailable);

    ResourceSnapshot {
        cpu_percent: cpu_percent.clamp(0.0, 100.0),
        core_equivalent_percent: core_equivalent_percent.max(0.0),
        memory_bytes,
        memory_metric: resource_memory_metric(),
        process_count,
        process_count_value_state: ResourceMetricValueState::Observed,
        process_ids,
        processes,
        metrics_unavailable: accounting
            .as_ref()
            .is_some_and(|snapshot| snapshot.metrics_unavailable)
            || accounting_error.is_some(),
        metrics_status: accounting
            .as_ref()
            .map(|snapshot| snapshot.status)
            .or_else(|| {
                accounting_error
                    .as_ref()
                    .map(|_| ProcessMetricStatus::Failed)
            })
            .unwrap_or(ProcessMetricStatus::Unknown),
        metric_values: combined_metric_state(cpu_value_state, memory_value_state),
        cpu_value_state,
        memory_value_state,
        metrics_stale: false,
        metrics_error: accounting_error.or(accounting_diagnostic),
        sampling_generation: accounting
            .as_ref()
            .map(|snapshot| snapshot.generation)
            .unwrap_or_default(),
        io_read_bytes: accounting
            .as_ref()
            .and_then(|snapshot| snapshot.io_read_bytes),
        io_write_bytes: accounting
            .as_ref()
            .and_then(|snapshot| snapshot.io_write_bytes),
        logical_cpu_count: logical_cpu_count.max(1),
        managed_process_fence: None,
        last_sample_at: Some(sampled_at),
    }
}

fn current_metric_state(
    members: &[ProcessAccountingMemberSnapshot],
    is_observed: impl Fn(&ProcessAccountingMemberSnapshot) -> bool,
) -> ResourceMetricValueState {
    if members.is_empty() {
        return ResourceMetricValueState::Observed;
    }
    let observed = members.iter().filter(|member| is_observed(member)).count();
    match observed {
        0 => ResourceMetricValueState::Unavailable,
        count if count == members.len() => ResourceMetricValueState::Observed,
        _ => ResourceMetricValueState::Partial,
    }
}

fn combined_metric_state(
    cpu: ResourceMetricValueState,
    memory: ResourceMetricValueState,
) -> ResourceMetricValueState {
    match (cpu, memory) {
        (ResourceMetricValueState::Unavailable, ResourceMetricValueState::Unavailable) => {
            ResourceMetricValueState::Unavailable
        }
        (ResourceMetricValueState::LastKnown, ResourceMetricValueState::LastKnown) => {
            ResourceMetricValueState::LastKnown
        }
        (ResourceMetricValueState::Observed, ResourceMetricValueState::Observed) => {
            ResourceMetricValueState::Observed
        }
        _ => ResourceMetricValueState::Partial,
    }
}

fn last_known_metric_state(state: ResourceMetricValueState) -> ResourceMetricValueState {
    match state {
        ResourceMetricValueState::Observed
        | ResourceMetricValueState::Partial
        | ResourceMetricValueState::LastKnown => ResourceMetricValueState::LastKnown,
        ResourceMetricValueState::Unavailable => ResourceMetricValueState::Unavailable,
    }
}

fn fixed_sampler_error_code(error: &SamplerError) -> &'static str {
    match error {
        SamplerError::InvalidLogicalProcessorCount => "sampler_invalid_cpu_count",
        SamplerError::InvalidInterval => "sampler_invalid_interval",
        SamplerError::CounterReset { .. } => "sampler_counter_reset",
        SamplerError::ConflictingProcessIdentity { .. } => "sampler_identity_conflict",
        SamplerError::WorkBudgetExceeded { .. } => "sampling_deadline_or_member_limit",
        SamplerError::ObservationFailed { .. } => "sampler_observation_failed",
    }
}

fn job_query_diagnostic_code(error: &str) -> &'static str {
    if error.contains("budget") || error.contains("exceeds") {
        "sampling_deadline_or_member_limit"
    } else {
        "job_query_unavailable"
    }
}

fn fixed_job_failure_code(code: Option<&str>) -> &'static str {
    match code {
        Some("job_authority_unavailable") => "job_authority_unavailable",
        Some("sampling_deadline_or_member_limit") => "sampling_deadline_or_member_limit",
        Some("sampling_deadline_exceeded") => "sampling_deadline_exceeded",
        Some("job_query_unavailable") | None => "job_query_unavailable",
        // All upstream errors are normalized before projection. An unknown
        // value can only be internal drift and must not cross the boundary.
        Some(_) => "job_query_unavailable",
    }
}

const MAX_COMMAND_ARGUMENTS: usize = 64;
const MAX_COMMAND_ARGUMENT_BYTES: usize = 4096;

fn bounded_command_shape(process: &sysinfo::Process) -> (Vec<String>, u16, u32) {
    let command = process.cmd();
    let argument_count = command.len().min(u16::MAX as usize) as u16;
    let mut argument_bytes = 0usize;
    let mut bounded = Vec::with_capacity(command.len().min(MAX_COMMAND_ARGUMENTS));
    for argument in command.iter().take(MAX_COMMAND_ARGUMENTS) {
        if argument_bytes >= MAX_COMMAND_ARGUMENT_BYTES {
            break;
        }
        let text = argument.to_string_lossy();
        let remaining = MAX_COMMAND_ARGUMENT_BYTES - argument_bytes;
        let mut bounded_text = String::new();
        for character in text.chars() {
            if bounded_text.len().saturating_add(character.len_utf8()) > remaining {
                break;
            }
            bounded_text.push(character);
        }
        argument_bytes = argument_bytes.saturating_add(bounded_text.len());
        bounded.push(bounded_text);
    }
    (
        bounded,
        argument_count,
        argument_bytes.min(u32::MAX as usize) as u32,
    )
}

fn redacted_executable_basename(path: &Path) -> Option<String> {
    const MAX_BYTES: usize = 96;
    let basename = path.file_name()?.to_string_lossy();
    let mut output = String::with_capacity(basename.len().min(MAX_BYTES));
    for character in basename.chars() {
        if output.len() >= MAX_BYTES {
            break;
        }
        output.push(
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            },
        );
    }
    (!output.is_empty()).then_some(output)
}

fn opaque_resource_id(resource_id: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    resource_id.hash(&mut hasher);
    format!("resource-{:016x}", hasher.finish())
}

fn process_resource_lifecycle(
    session_lifecycle: ProcessResourceLifecycle,
    authoritative_member: Option<&JobMemberObservation>,
) -> ProcessResourceLifecycle {
    match authoritative_member {
        // Metric availability is independent from lifecycle. An exact current
        // Job member keeps the owning session's Starting/Running/Stopping
        // state through first-baseline, counter-reset, and metadata gaps.
        Some(JobMemberObservation::Accessible { .. }) => session_lifecycle,
        Some(JobMemberObservation::Inaccessible { .. }) | None => ProcessResourceLifecycle::Unknown,
    }
}

fn resource_kind_label(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Shell => "terminal",
        SessionKind::Server => "service",
        SessionKind::Claude => "claude",
        SessionKind::Codex => "codex",
        SessionKind::Ssh => "ssh",
    }
}

fn sanitize_resource_kind(kind: Option<&str>) -> Option<String> {
    match kind {
        Some("terminal") => Some("terminal".to_string()),
        Some("service") => Some("service".to_string()),
        Some("claude") => Some("claude".to_string()),
        Some("codex") => Some("codex".to_string()),
        Some("ssh") => Some("ssh".to_string()),
        _ => None,
    }
}

fn resource_memory_metric() -> ResourceMemoryMetric {
    if cfg!(target_os = "windows") {
        ResourceMemoryMetric::PrivateCommitted
    } else {
        ResourceMemoryMetric::PrivateResident
    }
}

fn resolve_logical_cpu_count() -> u32 {
    platform_service::logical_processor_count()
}

fn unique_job_member_pids(job_members: &[JobMemberObservation]) -> Vec<u32> {
    job_members
        .iter()
        .map(|member| match member {
            JobMemberObservation::Accessible { identity } => identity.id().pid(),
            JobMemberObservation::Inaccessible { pid, .. } => *pid,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn job_member_to_process_observation(member: &JobMemberObservation) -> ProcessMemberObservation {
    match member {
        JobMemberObservation::Accessible { identity } => {
            ProcessSampler::observe_process_with_expected_identity(
                identity.id().pid(),
                Some(identity),
            )
        }
        JobMemberObservation::Inaccessible {
            pid,
            creation_time_100ns,
            reason,
        } => ProcessMemberObservation::Inaccessible(
            InaccessibleProcess::new(*pid, *creation_time_100ns).with_reason(reason.clone()),
        ),
    }
}

fn classify_process_display_name(process_name: &str, cmd: &[String]) -> String {
    let args_lower: Vec<String> = cmd.iter().map(|arg| arg.to_ascii_lowercase()).collect();

    let matches_token = |arg: &str, token: &str| -> bool {
        arg == token
            || arg.ends_with(&format!("/{token}"))
            || arg.ends_with(&format!("\\{token}"))
            || arg.contains(&format!("/{token}/"))
            || arg.contains(&format!("\\{token}\\"))
    };

    if args_lower
        .iter()
        .any(|arg| arg.contains("tinypool") && arg.contains("entry"))
    {
        return "Vitest worker".to_string();
    }
    if args_lower.iter().any(|arg| {
        let basename = Path::new(arg)
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        basename.starts_with("vitest") || matches_token(arg, "vitest")
    }) {
        return "Vitest".to_string();
    }
    if args_lower
        .iter()
        .any(|arg| arg.contains("@upstash/context7-mcp") || arg.contains("context7-mcp"))
    {
        return "Context7 MCP".to_string();
    }
    if args_lower.iter().any(|arg| {
        Path::new(arg)
            .file_name()
            .map(|name| name.to_string_lossy().eq_ignore_ascii_case("npm-cli.js"))
            .unwrap_or(false)
    }) {
        return "npm".to_string();
    }
    if args_lower.iter().any(|arg| {
        Path::new(arg)
            .file_name()
            .map(|name| name.to_string_lossy().eq_ignore_ascii_case("npx-cli.js"))
            .unwrap_or(false)
    }) {
        return "npx".to_string();
    }

    allowlisted_process_label(process_name)
}

fn allowlisted_process_label(process_name: &str) -> String {
    let basename = Path::new(process_name)
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| process_name.to_ascii_lowercase());
    match basename.trim_end_matches(".exe") {
        "node" => "Node".to_string(),
        "python" | "python3" => "Python".to_string(),
        "cargo" => "Cargo".to_string(),
        "rustc" => "Rust compiler".to_string(),
        "cmd" => "Command shell".to_string(),
        "powershell" | "pwsh" => "PowerShell".to_string(),
        "bash" | "sh" | "zsh" => "Shell".to_string(),
        "claude" => "Claude Code".to_string(),
        "codex" => "Codex".to_string(),
        "cursor" => "Cursor".to_string(),
        "devmanager" => "DevManager".to_string(),
        _ => "Other process".to_string(),
    }
}

fn normalize_process_name_for_detection(name: &str) -> String {
    name.trim().trim_end_matches(".exe").to_ascii_lowercase()
}

fn is_blocking_external_editor_name(name: &str) -> bool {
    matches!(
        normalize_process_name_for_detection(name).as_str(),
        "code"
            | "code-insiders"
            | "cursor"
            | "windsurf"
            | "notepad"
            | "notepad++"
            | "sublime_text"
            | "devenv"
            | "gvim"
            | "nvim-qt"
    )
}

fn ensure_prior_session_teardown_settled(
    inner: &Arc<ProcessManagerInner>,
    session_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let started_at = Instant::now();
    let mut last_close_error = None;
    loop {
        // A prelaunch runtime row can outlive both authoritative process
        // sources. Scrub its diagnostic projection before admission, but do
        // not turn owner/ledger absence into a fabricated lifecycle result.
        if try_admit_unowned_session_replacement(inner, session_id) {
            return Ok(());
        }
        if let Err(error) = retry_exact_session_teardown(inner, session_id) {
            last_close_error = Some(error);
        }
        // Launch preparation creates or updates the runtime projection before
        // the process operation executes.  A Starting row is therefore not
        // evidence of an old process owner.  Admission is safe once both
        // authoritative sources of process ownership are absent: no retained
        // TerminalSession/Job and no live ledger identity.
        if try_admit_unowned_session_replacement(inner, session_id) {
            return Ok(());
        }
        if started_at.elapsed() >= timeout {
            let suffix = last_close_error
                .map(|error| format!(" Last exact teardown error: {error}"))
                .unwrap_or_default();
            return Err(format!(
                "Prior managed terminal `{session_id}` did not settle before replacement.{suffix}"
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn retry_exact_session_teardown(
    inner: &Arc<ProcessManagerInner>,
    session_id: &str,
) -> Result<(), String> {
    let closed = close_exact_session_owner(inner, session_id, false)?;
    let _ = pid_file::prune_inactive_entries();
    if closed {
        mark_session_reaped(inner, session_id);
        return Ok(());
    }
    if session_projection_is_already_settled(inner, session_id) {
        return Ok(());
    }
    Err(format!(
        "Exact managed teardown authority for session `{session_id}` is unavailable"
    ))
}

fn session_has_no_process_authority_or_evidence(
    inner: &ProcessManagerInner,
    session_id: &str,
) -> bool {
    inner
        .sessions
        .lock()
        .map(|sessions| !sessions.contains_key(session_id))
        .unwrap_or(false)
        && pid_file::active_tracked_pids_for_session(session_id).is_empty()
}

fn try_admit_unowned_session_replacement(
    inner: &Arc<ProcessManagerInner>,
    session_id: &str,
) -> bool {
    if !session_has_no_process_authority_or_evidence(inner, session_id) {
        return false;
    }

    let mut changed = false;
    let mut runtime = match inner.runtime_state.write() {
        Ok(runtime) => runtime,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(session) = runtime.sessions.get_mut(session_id) {
        if session.reap_incomplete {
            return false;
        }
        let dirty_before = session.dirty_generation;
        session.pid = None;
        session.resources = ResourceSnapshot::default();
        session.mark_dirty();
        changed = session.dirty_generation != dirty_before;
    }
    drop(runtime);
    if changed {
        bump_runtime_revision(inner);
        mark_remote_session_dirty(inner, session_id);
        emit_tracked_remote_runtime_snapshot(inner, session_id);
    }
    true
}

fn session_projection_is_already_settled(inner: &ProcessManagerInner, session_id: &str) -> bool {
    let owner_absent = inner
        .sessions
        .lock()
        .map(|sessions| !sessions.contains_key(session_id))
        .unwrap_or(false);
    let runtime_settled = inner
        .runtime_state
        .read()
        .map(|runtime| {
            runtime
                .sessions
                .get(session_id)
                .map(|session| {
                    session.status == SessionStatus::Stopped
                        && !session.reap_incomplete
                        && session_process_projection_is_clean(session)
                })
                .unwrap_or(true)
        })
        .unwrap_or(false);
    owner_absent
        && runtime_settled
        && pid_file::active_tracked_pids_for_session(session_id).is_empty()
}

fn session_process_projection_is_clean(session: &SessionRuntimeState) -> bool {
    session.pid.is_none()
        && session.resources.cpu_percent == 0.0
        && session.resources.core_equivalent_percent == 0.0
        && session.resources.memory_bytes == 0
        && session.resources.process_count == 0
        && session.resources.process_ids.is_empty()
        && session.resources.processes.is_empty()
        && session.resources.managed_process_fence.is_none()
}

/// Close and remove only the exact TerminalSession observed before teardown.
/// Failed release or persistence leaves it retained for the same operation
/// and fence to retry; a concurrent replacement is never removed.
fn close_exact_session_owner(
    inner: &Arc<ProcessManagerInner>,
    session_id: &str,
    closed_by_user: bool,
) -> Result<bool, String> {
    let session = match inner.sessions.lock() {
        Ok(sessions) => sessions.get(session_id).cloned(),
        Err(_) => {
            clear_unowned_managed_process_projection(inner, session_id, closed_by_user);
            return Err("Session store poisoned".to_string());
        }
    };
    let Some(session) = session else {
        clear_unowned_managed_process_projection(inner, session_id, closed_by_user);
        return Ok(false);
    };
    #[cfg(windows)]
    {
        let fence = session
            .managed_process_fence()?
            .ok_or_else(|| "Managed terminal teardown authority is missing".to_string())?;
        session.close_managed_process_exact(&fence, closed_by_user)?;
    }
    #[cfg(not(windows))]
    session.close(closed_by_user)?;

    let removed = {
        let mut sessions = inner
            .sessions
            .lock()
            .map_err(|_| "Session store poisoned".to_string())?;
        match sessions.get(session_id) {
            Some(current) if Arc::ptr_eq(current, &session) => sessions.remove(session_id),
            Some(_) => {
                return Err(format!(
                    "Session `{session_id}` changed generations before exact owner release"
                ))
            }
            None => None,
        }
    };
    drop(removed);
    drop(session);
    Ok(true)
}

#[derive(Debug, Clone, Copy)]
struct IssuedTerminalResource {
    owner: ProcessOwner,
    resource_id: ResourceId,
    generation: u64,
}

#[derive(Debug)]
struct TerminalAuthorityState {
    next_action_epoch: u64,
    resources: HashMap<String, IssuedTerminalResource>,
    resource_order: VecDeque<String>,
    completion_store: Option<TeardownCompletionStore>,
}

#[derive(Debug)]
struct TerminalAuthorityIssuer {
    state: Mutex<TerminalAuthorityState>,
}

fn cleanup_claude_hook_session_inner(inner: &ProcessManagerInner, session_id: &str) {
    fence_and_remove_claude_hook_session(inner, session_id, None);
}

fn cleanup_codex_adapter_session_inner(inner: &ProcessManagerInner, session_id: &str) {
    let removed = inner
        .codex_adapter_registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove_session(session_id);
    let removed_identity = removed
        .as_ref()
        .and_then(|session| session.registered_semantic_identity(session_id));
    let relay_nonce = removed.as_ref().and_then(|session| match session {
        CodexAdapterSession::Running { registration, .. } => Some(registration.nonce.clone()),
        CodexAdapterSession::Pending(_) | CodexAdapterSession::Degraded(_) => None,
    });
    drop(removed);
    if let Some(nonce) = relay_nonce {
        inner.codex_hook_registry.unregister(&nonce);
    }
    if let Some(identity) = removed_identity {
        emit_remote_session_event(inner, RemoteSessionEvent::CodexAdapterRemoved { identity });
    }
}

fn cleanup_browser_provider_session_inner(inner: &ProcessManagerInner, session_id: &str) {
    let removed = inner
        .browser_provider_sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(session_id);
    if let Some(removed) = removed {
        removed.registrar.revoke(&removed.registration);
    }
}

fn cleanup_ai_adapters_for_session_inner(inner: &ProcessManagerInner, session_id: &str) {
    cleanup_claude_hook_session_inner(inner, session_id);
    cleanup_codex_adapter_session_inner(inner, session_id);
    cleanup_browser_provider_session_inner(inner, session_id);
}

#[derive(Debug, Default)]
struct ProviderRuntimeBook {
    live: HashMap<(ResourceId, u64), ProviderLiveSession>,
    recovery: HashMap<
        crate::providers::session::RecoveryKey,
        crate::process::registry::ProviderManagedProcessPermit,
    >,
    failures: VecDeque<ProviderSessionFailure>,
    latched_exact_resume_failures: HashMap<(ResourceId, u64), CodexExactResumeLaunchBinding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderSettlementKind {
    ObserveExit,
    AbortRejectedSessionStart,
}

#[derive(Debug, Clone)]
struct ProviderLiveSession {
    session_id: String,
    fence: ManagedProcessFence,
    correlation: crate::providers::session::RuntimeCorrelation,
    task_id: TaskId,
    agent_session_id: crate::domain::AgentSessionId,
    provider_kind: ProviderKind,
    provider_session_id: Option<crate::domain::ProviderSessionId>,
    /// Launch-time resume intent is not live identity proof. Only the
    /// authenticated current-generation SessionStart hook confirms it.
    provider_identity_confirmed: bool,
    /// Owns the one-shot durable SessionStart transaction. A duplicate hook or
    /// post-launch reconciliation may observe this claim but must not consume
    /// the authenticated provenance a second time.
    provider_identity_acceptance_started: bool,
    /// Claimed before exit settlement so notifier/background races cannot
    /// report the same provider terminal exit twice.
    exit_reported: bool,
    settlement_kind: ProviderSettlementKind,
    settlement_failures: u32,
    next_settlement_attempt: Option<Instant>,
    failure_reported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderSessionBinding {
    pub task_id: TaskId,
    pub agent_session_id: crate::domain::AgentSessionId,
    pub resource_id: ResourceId,
    pub provider_kind: ProviderKind,
    pub provider_session_id: crate::domain::ProviderSessionId,
    pub runtime_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderSessionFailure {
    pub task_id: TaskId,
    pub agent_session_id: crate::domain::AgentSessionId,
    pub provider_kind: ProviderKind,
    pub failure: crate::providers::session::ExactResumeFailure,
}

fn queue_provider_session_failure(inner: &ProcessManagerInner, failure: ProviderSessionFailure) {
    if let Ok(mut book) = inner.provider_runtime.lock() {
        if book.failures.len() >= MAX_PROVIDER_SESSION_FAILURES {
            book.failures.pop_front();
        }
        book.failures.push_back(failure);
    }
}

fn provider_exit_retry_delay(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(7);
    (PROVIDER_EXIT_RETRY_INITIAL * (1_u32 << exponent)).min(PROVIDER_EXIT_RETRY_MAX)
}

fn reconcile_provider_terminal_exits(inner: &Arc<ProcessManagerInner>) {
    let now = Instant::now();
    let candidates = inner
        .provider_runtime
        .lock()
        .map(|book| {
            book.live
                .values()
                .filter(|live| {
                    !live.exit_reported
                        && live
                            .next_settlement_attempt
                            .is_none_or(|deadline| deadline <= now)
                })
                .map(|live| live.session_id.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for session_id in candidates {
        if provider_terminal_has_exited(inner, &session_id) {
            reconcile_one_provider_terminal_exit(inner, &session_id);
        }
    }
}

fn provider_terminal_has_exited(inner: &ProcessManagerInner, session_id: &str) -> bool {
    inner
        .runtime_state
        .read()
        .ok()
        .and_then(|runtime| {
            runtime
                .sessions
                .get(session_id)
                .map(|state| !state.status.is_live())
        })
        .unwrap_or(true)
}

fn reconcile_one_provider_terminal_exit(inner: &ProcessManagerInner, session_id: &str) {
    let live = {
        let Ok(mut book) = inner.provider_runtime.lock() else {
            return;
        };
        let Some(live) = book
            .live
            .values_mut()
            .find(|live| live.session_id == session_id && !live.exit_reported)
        else {
            return;
        };
        live.exit_reported = true;
        live.provider_identity_confirmed = false;
        live.provider_identity_acceptance_started = matches!(
            live.settlement_kind,
            ProviderSettlementKind::AbortRejectedSessionStart
        );
        live.next_settlement_attempt = None;
        live.clone()
    };

    let settlement = inner
        .provider_sessions
        .lock()
        .map_err(|_| "provider session manager lock poisoned".to_string())
        .and_then(|mut slot| {
            let manager = slot
                .as_mut()
                .ok_or_else(|| "provider session manager is unavailable".to_string())?;
            match live.settlement_kind {
                ProviderSettlementKind::ObserveExit => manager
                    .process_exited(live.correlation)
                    .map_err(|error| error.to_string()),
                ProviderSettlementKind::AbortRejectedSessionStart => manager
                    .close_agent_session(live.agent_session_id)
                    .map_err(|error| error.to_string()),
            }
        });
    if let Err(error) = settlement {
        let mut attempt = 0;
        if let Ok(mut book) = inner.provider_runtime.lock() {
            if let Some(current) = book.live.values_mut().find(|current| {
                current.session_id == live.session_id && current.correlation == live.correlation
            }) {
                current.settlement_failures = current.settlement_failures.saturating_add(1);
                attempt = current.settlement_failures;
                current.next_settlement_attempt =
                    Some(Instant::now() + provider_exit_retry_delay(attempt));
                current.exit_reported = false;
            }
        }
        if attempt <= 1 || attempt.is_power_of_two() {
            eprintln!("provider terminal exit settlement deferred (attempt {attempt}): {error}");
        }
        return;
    }

    if !live.failure_reported {
        queue_provider_session_failure(
            inner,
            ProviderSessionFailure {
                task_id: live.task_id,
                agent_session_id: live.agent_session_id,
                provider_kind: live.provider_kind,
                failure: crate::providers::session::ExactResumeFailure::ProviderRejected,
            },
        );
        if let Ok(mut book) = inner.provider_runtime.lock() {
            if let Some(current) = book.live.values_mut().find(|current| {
                current.session_id == live.session_id && current.correlation == live.correlation
            }) {
                current.failure_reported = true;
            }
        }
    }
}

fn take_latched_codex_exact_resume_failure(
    inner: &ProcessManagerInner,
    key: (ResourceId, u64),
) -> Option<CodexExactResumeLaunchBinding> {
    inner
        .provider_runtime
        .lock()
        .ok()
        .and_then(|mut book| book.latched_exact_resume_failures.remove(&key))
}

impl TerminalAuthorityIssuer {
    fn new() -> Self {
        Self {
            state: Mutex::new(TerminalAuthorityState {
                next_action_epoch: 1,
                resources: HashMap::new(),
                resource_order: VecDeque::with_capacity(MAX_TERMINAL_AUTHORITY_RESOURCES),
                completion_store: None,
            }),
        }
    }

    fn issue(
        &self,
        session_id: &str,
        owner: ProcessOwner,
        ports: Vec<u16>,
    ) -> Result<TerminalLaunchAuthority, String> {
        if session_id.trim().is_empty() || session_id.len() > 256 {
            return Err("terminal authority session identity is invalid".to_string());
        }
        if ports.len() > MAX_MANAGED_TERMINAL_PORTS {
            return Err(format!(
                "terminal launch port set exceeds {MAX_MANAGED_TERMINAL_PORTS} entries"
            ));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "terminal authority issuer poisoned".to_string())?;
        state
            .resource_order
            .retain(|retained| retained != session_id);
        if !state.resources.contains_key(session_id) {
            while state.resources.len() >= MAX_TERMINAL_AUTHORITY_RESOURCES {
                let Some(evicted) = state.resource_order.pop_front() else {
                    return Err("terminal authority retention index is inconsistent".to_string());
                };
                state.resources.remove(&evicted);
            }
        }
        state.resource_order.push_back(session_id.to_string());
        let action_epoch = state.next_action_epoch;
        state.next_action_epoch = state
            .next_action_epoch
            .checked_add(1)
            .ok_or_else(|| "terminal action epoch space is exhausted".to_string())?;

        let issued = match state.resources.get(session_id).copied() {
            Some(current) if current.owner == owner => IssuedTerminalResource {
                generation: current
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| "terminal runtime generation is exhausted".to_string())?,
                ..current
            },
            _ => IssuedTerminalResource {
                owner,
                resource_id: ResourceId::new(),
                generation: 1,
            },
        };
        state.resources.insert(session_id.to_string(), issued);
        if state.completion_store.is_none() {
            #[cfg(windows)]
            {
                state.completion_store = Some(TeardownCompletionStore::for_terminal_host()?);
            }
            #[cfg(not(windows))]
            {
                state.completion_store = Some(TeardownCompletionStore::new());
            }
        }
        let completion_store = state
            .completion_store
            .as_ref()
            .expect("terminal completion store initialized")
            .clone();
        TerminalLaunchAuthority::new(
            issued.owner,
            issued.resource_id,
            issued.generation,
            OperationId::new(),
            action_epoch,
            ports,
            completion_store,
        )
    }

    fn issue_exact(
        &self,
        session_id: &str,
        owner: ProcessOwner,
        resource_id: ResourceId,
        generation: u64,
        action_epoch: u64,
        ports: Vec<u16>,
    ) -> Result<TerminalLaunchAuthority, String> {
        if session_id.trim().is_empty() || session_id.len() > 256 {
            return Err("terminal authority session identity is invalid".to_string());
        }
        if generation == 0 || action_epoch == 0 {
            return Err("terminal launch generation and action epoch must be non-zero".to_string());
        }
        if ports.len() > MAX_MANAGED_TERMINAL_PORTS {
            return Err(format!(
                "terminal launch port set exceeds {MAX_MANAGED_TERMINAL_PORTS} entries"
            ));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "terminal authority issuer poisoned".to_string())?;
        state
            .resource_order
            .retain(|retained| retained != session_id);
        if !state.resources.contains_key(session_id) {
            while state.resources.len() >= MAX_TERMINAL_AUTHORITY_RESOURCES {
                let Some(evicted) = state.resource_order.pop_front() else {
                    return Err("terminal authority retention index is inconsistent".to_string());
                };
                state.resources.remove(&evicted);
            }
        }
        state.resource_order.push_back(session_id.to_string());
        state.next_action_epoch = state.next_action_epoch.max(action_epoch.saturating_add(1));
        state.resources.insert(
            session_id.to_string(),
            IssuedTerminalResource {
                owner,
                resource_id,
                generation,
            },
        );
        if state.completion_store.is_none() {
            #[cfg(windows)]
            {
                state.completion_store = Some(TeardownCompletionStore::for_terminal_host()?);
            }
            #[cfg(not(windows))]
            {
                state.completion_store = Some(TeardownCompletionStore::new());
            }
        }
        let completion_store = state
            .completion_store
            .as_ref()
            .expect("terminal completion store initialized")
            .clone();
        TerminalLaunchAuthority::new(
            owner,
            resource_id,
            generation,
            OperationId::new(),
            action_epoch,
            ports,
            completion_store,
        )
    }
}

fn restart_history_text(snapshot: &TerminalScreenSnapshot) -> String {
    let estimated = snapshot
        .lines
        .len()
        .saturating_mul(snapshot.cols.saturating_add(2))
        .min(MAX_RESTART_HISTORY_BYTES);
    let mut text = String::with_capacity(estimated);

    'lines: for line in &snapshot.lines {
        let line_start = text.len();
        for cell in line {
            let character = if cell.character == '\u{00a0}' {
                ' '
            } else {
                cell.character
            };
            if text.len().saturating_add(character.len_utf8()) > MAX_RESTART_HISTORY_BYTES {
                break 'lines;
            }
            text.push(character);
        }
        while text.len() > line_start && text.ends_with(' ') {
            text.pop();
        }
        if text.len().saturating_add(2) > MAX_RESTART_HISTORY_BYTES {
            break;
        }
        text.push_str("\r\n");
    }

    while text.ends_with("\r\n") {
        text.truncate(text.len().saturating_sub(2));
    }
    text
}

fn mark_session_reaped(inner: &ProcessManagerInner, session_id: &str) {
    if inner
        .sessions
        .lock()
        .map(|sessions| sessions.contains_key(session_id))
        .unwrap_or(true)
    {
        // A retained session owns a retryable exact teardown. PID absence is
        // not enough to publish Stopped before registry release and durable
        // settlement have succeeded.
        return;
    }
    let mut changed = false;
    if let Ok(mut runtime) = inner.runtime_state.write() {
        if let Some(session) = runtime.sessions.get_mut(session_id) {
            if session.status != SessionStatus::Stopped || session.reap_incomplete {
                let dirty_before = session.dirty_generation;
                session.status = SessionStatus::Stopped;
                session.pid = None;
                session.resources = ResourceSnapshot::default();
                session.reap_incomplete = false;
                session.clear_user_exit_requests();
                if session.exit.is_none() {
                    session.exit = Some(SessionExitState {
                        code: None,
                        signal: None,
                        closed_by_user: true,
                        summary: "Session processes cleared.".to_string(),
                    });
                }
                session.mark_dirty();
                changed = session.dirty_generation != dirty_before;
            }
        }
    }
    if changed {
        bump_runtime_revision(inner);
        emit_tracked_remote_runtime_snapshot(inner, session_id);
    }
}

fn reconcile_exit_states(inner: &Arc<ProcessManagerInner>) {
    #[derive(Debug)]
    enum ExitReconciliation {
        RestoreInterruptedServer {
            session_id: String,
            cwd: PathBuf,
            dimensions: SessionDimensions,
        },
        MarkStopped {
            session_id: String,
        },
        MarkCrashed {
            session_id: String,
        },
    }

    let now = Instant::now();
    let mut actions = Vec::new();
    if let Ok(runtime) = inner.runtime_state.read() {
        for (id, session) in &runtime.sessions {
            if matches!(
                session.status,
                SessionStatus::Exited | SessionStatus::Failed
            ) && (session.command_id.is_some()
                || session.session_kind.is_ai()
                || matches!(session.session_kind, SessionKind::Ssh))
            {
                let closed_by_user = session
                    .exit
                    .as_ref()
                    .map(|exit| exit.closed_by_user)
                    .unwrap_or(false);
                let requested_stop = closed_by_user || session.has_recent_user_stop_request(now);
                if matches!(session.session_kind, SessionKind::Server)
                    && session.has_recent_user_interrupt(now)
                {
                    actions.push(ExitReconciliation::RestoreInterruptedServer {
                        session_id: id.clone(),
                        cwd: session.cwd.clone(),
                        dimensions: session.dimensions,
                    });
                } else if requested_stop {
                    actions.push(ExitReconciliation::MarkStopped {
                        session_id: id.clone(),
                    });
                } else {
                    actions.push(ExitReconciliation::MarkCrashed {
                        session_id: id.clone(),
                    });
                }
            }
        }
    }

    if actions.is_empty() {
        return;
    }

    for action in actions {
        match action {
            ExitReconciliation::RestoreInterruptedServer {
                session_id,
                cwd,
                dimensions,
            } => {
                if retry_exact_session_teardown(inner, &session_id).is_ok()
                    && restore_interrupted_server_prompt(inner, &session_id, cwd, dimensions)
                        .is_err()
                {
                    mark_session_reaped(inner, &session_id);
                }
            }
            ExitReconciliation::MarkStopped { session_id } => {
                if retry_exact_session_teardown(inner, &session_id).is_ok() {
                    mark_session_reaped(inner, &session_id);
                }
            }
            ExitReconciliation::MarkCrashed { session_id } => {
                let _ = retry_exact_session_teardown(inner, &session_id);
                let mut changed = false;
                if let Ok(mut runtime) = inner.runtime_state.write() {
                    if let Some(session) = runtime.sessions.get_mut(&session_id) {
                        let dirty_before = session.dirty_generation;
                        session.status = SessionStatus::Crashed;
                        session.clear_user_exit_requests();
                        session.mark_dirty();
                        changed = session.dirty_generation != dirty_before;
                    }
                }
                if changed {
                    bump_runtime_revision(inner);
                    emit_tracked_remote_runtime_snapshot(inner, &session_id);
                }
            }
        }
    }
}

fn reconcile_ai_activity(inner: &Arc<ProcessManagerInner>) {
    let notification_sound = inner
        .notification_sound
        .read()
        .map(|sound| sound.clone())
        .unwrap_or(None);
    let mut should_notify = false;
    let now = Instant::now();

    if let Ok(mut runtime) = inner.runtime_state.write() {
        let active_session_id = runtime.active_session_id.clone();
        let mut touched_sessions = Vec::new();
        for (session_id, session) in &mut runtime.sessions {
            let gen_before = session.dirty_generation;
            session.reconcile_ai_idle(active_session_id.as_deref(), now);
            let mut changed = session.dirty_generation != gen_before;

            match session.check_pending_notification(now) {
                AiIdleTransition::BackgroundReady | AiIdleTransition::ForegroundReady => {
                    should_notify = true;
                    session.notification_count += 1;
                    changed = true;
                }
                AiIdleTransition::NoChange => {}
            }

            if changed {
                touched_sessions.push(session_id.clone());
            }
        }
        drop(runtime);
        if !touched_sessions.is_empty() {
            bump_runtime_revision(inner);
        }
        for session_id in touched_sessions {
            emit_tracked_remote_runtime_snapshot(inner, &session_id);
        }
    }

    if should_notify {
        notifications::play_notification_sound(notification_sound.as_deref());
    }
}

fn handle_auto_restart(inner: &Arc<ProcessManagerInner>) {
    let mut restart_candidates = Vec::with_capacity(MAX_AUTO_RESTART_WORKERS);
    if let Ok(runtime) = inner.runtime_state.read() {
        for session in runtime.sessions.values() {
            if session.auto_restart
                && matches!(session.status, SessionStatus::Crashed)
                && session.server_launch.is_some()
            {
                restart_candidates.push(session.server_launch.clone().unwrap());
                if restart_candidates.len() == MAX_AUTO_RESTART_WORKERS {
                    break;
                }
            }
        }
    }

    if restart_candidates.is_empty() {
        return;
    }

    for launch in restart_candidates {
        {
            let mut workers = inner
                .auto_restart_workers
                .lock()
                .unwrap_or_else(|_| std::process::abort());
            let mut index = 0usize;
            while index < workers.len() {
                if workers[index].is_finished() {
                    let finished = workers.swap_remove(index);
                    join_process_manager_helper(finished);
                } else {
                    index += 1;
                }
            }
            if workers.len() >= MAX_AUTO_RESTART_WORKERS {
                break;
            }
        }
        let delay = {
            let mut backoffs = inner
                .restart_backoffs
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let now = Instant::now();
            let entry = backoffs
                .entry(launch.command_id.clone())
                .or_insert(RestartBackoff {
                    delay: Duration::from_secs(1),
                    last_crash: now,
                });
            if now.duration_since(entry.last_crash) < Duration::from_secs(60) {
                entry.delay = std::cmp::min(entry.delay * 2, Duration::from_secs(30));
            } else {
                entry.delay = Duration::from_secs(1);
            }
            entry.last_crash = now;
            entry.delay
        };

        let launch_id = launch.command_id.clone();
        let mut changed = false;
        if let Ok(mut runtime) = inner.runtime_state.write() {
            if let Some(session) = runtime.sessions.get_mut(&launch_id) {
                let dirty_before = session.dirty_generation;
                session.status = SessionStatus::Starting;
                session.exit = Some(SessionExitState {
                    code: None,
                    signal: None,
                    closed_by_user: false,
                    summary: format!("Auto-restarting in {}s", delay.as_secs().max(1)),
                });
                session.mark_dirty();
                changed = session.dirty_generation != dirty_before;
            }
        }
        if changed {
            bump_runtime_revision(&inner);
            emit_tracked_remote_runtime_snapshot(&inner, &launch_id);
        }

        let launch_clone = launch.clone();
        let weak_inner = Arc::downgrade(inner);
        let worker = thread::spawn(move || {
            let delay_started = Instant::now();
            while delay_started.elapsed() < delay {
                let Some(inner) = weak_inner.upgrade() else {
                    return;
                };
                if inner.background_stop.load(Ordering::SeqCst) {
                    return;
                }
                drop(inner);
                thread::sleep(
                    delay
                        .saturating_sub(delay_started.elapsed())
                        .min(Duration::from_millis(25)),
                );
            }
            let Some(inner) = weak_inner.upgrade() else {
                return;
            };
            if inner.background_stop.load(Ordering::SeqCst) {
                return;
            }
            #[cfg(test)]
            let worker_test_hook = inner
                .auto_restart_worker_test_hook
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            #[cfg(test)]
            if let Some(hook) = worker_test_hook.as_ref() {
                hook(AutoRestartWorkerTestPhase::BeforeQueueAdmission);
            }
            if inner.background_stop.load(Ordering::SeqCst) {
                return;
            }
            let op_queue = inner.op_queue.lock().ok().and_then(|queue| queue.upgrade());
            let Some(op_queue) = op_queue else {
                return;
            };
            #[cfg(test)]
            if let Some(hook) = worker_test_hook.as_ref() {
                hook(AutoRestartWorkerTestPhase::AfterQueueLease);
            }
            drop(inner);
            let op_id = next_op_id();
            if op_queue
                .submit(ProcessOp::StartServer {
                    op_id,
                    launch: launch_clone,
                    dimensions: SessionDimensions::default(),
                    activate: false,
                    response: None,
                })
                .is_ok()
            {
                #[cfg(test)]
                if let Some(hook) = worker_test_hook.as_ref() {
                    hook(AutoRestartWorkerTestPhase::AfterEffect);
                }
            }
        });
        let mut workers = inner
            .auto_restart_workers
            .lock()
            .unwrap_or_else(|_| std::process::abort());
        workers.push(worker);
    }
}

fn build_command_env(folder: &ProjectFolder, command: &RunCommand) -> HashMap<String, String> {
    let mut env = HashMap::new();

    if let Some(env_file_path) = folder.env_file_path.as_deref() {
        let env_path = PathBuf::from(&folder.folder_path).join(env_file_path);
        if let Ok(file_env) = env_service::read_env_map(&env_path) {
            env.extend(file_env);
        }
    }

    if let Some(command_env) = command.env.as_ref() {
        for (key, value) in command_env {
            env.insert(key.clone(), value.clone());
        }
    }

    env
}

fn build_server_log_file_path(
    project: &Project,
    folder: &ProjectFolder,
    _command: &RunCommand,
) -> Option<PathBuf> {
    if project.save_log_files == Some(false) {
        return None;
    }

    let root = PathBuf::from(&project.root_path);
    if !root.is_dir() {
        return None;
    }

    let folder_name = Path::new(&folder.folder_path)
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "server".to_string());
    let slug = folder_name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    let file_name = if slug.is_empty() {
        "log-server.log".to_string()
    } else {
        format!("log-{slug}.log")
    };
    Some(root.join(file_name))
}

fn build_server_launch_command(settings: &Settings, command: &RunCommand) -> (String, Vec<String>) {
    if cfg!(target_os = "windows") {
        let mut args = vec!["/C".to_string(), command.command.clone()];
        args.extend(command.args.clone());
        return ("cmd".to_string(), args);
    }

    let shell = resolve_shell_path(settings);
    let args = vec![
        "-l".to_string(),
        "-c".to_string(),
        build_shell_command_line(command),
    ];

    (shell, args)
}

/// OpenSSH rejects key files with CRLF line endings or a missing final
/// newline — both are common artifacts of pasting a key into a text field.
fn sanitize_private_key(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    format!("{}\n", normalized.trim())
}

fn safe_key_file_name(connection_id: &str) -> String {
    connection_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn materialize_ssh_key_in(
    dir: &Path,
    connection: &SSHConnection,
) -> Result<Option<PathBuf>, String> {
    let Some(key) = connection
        .private_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
    else {
        return Ok(None);
    };

    let file_name = safe_key_file_name(&connection.id);
    if file_name.is_empty() {
        return Err("connection id is empty".to_string());
    }

    std::fs::create_dir_all(dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("set permissions on {}: {error}", dir.display()))?;
    }
    let path = dir.join(file_name);
    write_key_file(&path, &sanitize_private_key(key))?;
    if let Err(error) = lock_key_file_permissions(&path) {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(Some(path))
}

#[cfg(unix)]
fn write_key_file(path: &Path, contents: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(contents.as_bytes()))
        .map_err(|error| format!("write {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn write_key_file(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|error| format!("write {}: {error}", path.display()))
}

#[cfg(unix)]
fn lock_key_file_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("set permissions on {}: {error}", path.display()))
}

#[cfg(windows)]
fn lock_key_file_permissions(path: &Path) -> Result<(), String> {
    // Win32-OpenSSH refuses private keys readable by other accounts. Strip
    // inherited ACEs and grant only the current user.
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let username =
        std::env::var("USERNAME").map_err(|_| "resolve current user name".to_string())?;
    let output = std::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{username}:F"))
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("run icacls on {}: {error}", path.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "icacls failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn lock_key_file_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn build_ssh_launch_spec(
    app_state: &AppState,
    tab: &SessionTab,
    connection: &SSHConnection,
    key_file: Option<&Path>,
) -> SshLaunchSpec {
    let cwd = app_state
        .find_project(&tab.project_id)
        .map(|project| PathBuf::from(&project.root_path))
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));

    let mut args = vec![
        format!("{}@{}", connection.username.trim(), connection.host.trim()),
        "-p".to_string(),
        connection.port.to_string(),
    ];
    if let Some(key_file) = key_file {
        // No `-o IdentitiesOnly=yes` on purpose: the user prefers the saved
        // key but still wants agent/default keys as fallback.
        args.push("-i".to_string());
        args.push(key_file.display().to_string());
    }

    SshLaunchSpec {
        tab_id: tab.id.clone(),
        ssh_connection_id: connection.id.clone(),
        project_id: tab.project_id.clone(),
        cwd,
        program: "ssh".to_string(),
        args,
    }
}

fn build_ai_launch_spec(
    settings: &Settings,
    project: &Project,
    tab: &SessionTab,
    session_id: &str,
) -> Result<AiLaunchSpec, String> {
    let cwd = PathBuf::from(&project.root_path);
    let cwd = if cwd.is_dir() {
        cwd
    } else {
        std::env::current_dir().unwrap_or_else(|_| ".".into())
    };
    let (shell_program, shell_args) = build_interactive_shell_command(settings);
    let configured = resolve_ai_startup_command(settings, tab.tab_type.clone())?;
    let shell = claude_shell_kind(&shell_program);
    let startup_command = adapt_ai_startup_command(configured, tab, shell)?;

    let launch = AiLaunchSpec {
        tab_id: tab.id.clone(),
        project_id: tab.project_id.clone(),
        tool: match tab.tab_type {
            TabType::Claude => SessionKind::Claude,
            TabType::Codex => SessionKind::Codex,
            _ => return Err(format!("Unsupported AI tab type `{}`", tab.id)),
        },
        cwd,
        shell_program,
        shell_args,
        startup_command,
    };

    if session_id.is_empty() {
        return Err("AI session id cannot be empty".to_string());
    }

    Ok(launch)
}

const MAX_PROVIDER_SESSION_ID_LEN: usize = 256;

enum AiResume<'a> {
    Fresh,
    Exact(&'a str),
    Picker,
}

fn validate_provider_session_id(id: &str) -> Result<&str, String> {
    if id.is_empty()
        || id.len() > MAX_PROVIDER_SESSION_ID_LEN
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
    {
        return Err("provider session id is invalid".to_string());
    }
    Ok(id)
}

fn adapt_ai_startup_command(
    configured: String,
    tab: &SessionTab,
    shell: ClaudeShellKind,
) -> Result<String, String> {
    let resume = match (
        tab.provider_session_id.as_deref(),
        tab.pty_session_id.as_deref(),
    ) {
        (Some(id), _) => AiResume::Exact(validate_provider_session_id(id)?),
        (None, None) => AiResume::Picker,
        (None, Some(_)) => AiResume::Fresh,
    };

    match resume {
        AiResume::Fresh => Ok(configured),
        AiResume::Picker => Ok(append_ai_resume_args(
            &configured,
            tab.tab_type.clone(),
            None,
            shell,
        )),
        AiResume::Exact(id) => Ok(append_ai_resume_args(
            &configured,
            tab.tab_type.clone(),
            Some(id),
            shell,
        )),
    }
}

fn append_ai_resume_args(
    configured: &str,
    tab_type: TabType,
    provider_session_id: Option<&str>,
    shell: ClaudeShellKind,
) -> String {
    let mut command = configured.to_string();
    if !command.ends_with(char::is_whitespace) {
        command.push(' ');
    }
    match tab_type {
        TabType::Claude => {
            command.push_str("--resume");
            if let Some(id) = provider_session_id {
                command.push(' ');
                command.push_str(&quote_shell_argument(id, shell));
            }
        }
        TabType::Codex => {
            command.push_str("resume");
            if let Some(id) = provider_session_id {
                command.push(' ');
                command.push_str(&quote_shell_argument(id, shell));
            }
        }
        _ => {}
    }
    command
}

fn windows_shell_for(
    terminal: &crate::models::DefaultTerminal,
    shell_integration: bool,
    pwsh: Option<std::path::PathBuf>,
) -> (String, Vec<String>) {
    match terminal {
        crate::models::DefaultTerminal::Powershell => ("powershell.exe".to_string(), Vec::new()),
        crate::models::DefaultTerminal::Pwsh => match pwsh {
            Some(path) => (path.to_string_lossy().into_owned(), Vec::new()),
            // Selected pwsh but it is gone (uninstalled, hand-edited config):
            // degrade to Windows PowerShell rather than failing the launch.
            None => ("powershell.exe".to_string(), Vec::new()),
        },
        crate::models::DefaultTerminal::Cmd => ("cmd.exe".to_string(), Vec::new()),
        crate::models::DefaultTerminal::Bash => (
            preferred_windows_bash_program(),
            bash_shell_args(shell_integration),
        ),
    }
}

fn build_interactive_shell_command(settings: &Settings) -> (String, Vec<String>) {
    if cfg!(target_os = "windows") {
        return windows_shell_for(
            &settings.default_terminal,
            settings.shell_integration_enabled,
            crate::services::pwsh_probe::pwsh_program(),
        );
    }

    if cfg!(target_os = "macos") {
        // On macOS the default_terminal setting (Bash/Powershell/Cmd) doesn't apply.
        // resolve_shell_path honors mac_terminal_profile and falls back to $SHELL/zsh,
        // avoiding the bundled bash 3.2.
        let shell = resolve_shell_path(settings);
        return (shell, vec!["-l".to_string()]);
    }

    match settings.default_terminal.clone() {
        crate::models::DefaultTerminal::Bash => (
            "bash".to_string(),
            bash_shell_args(settings.shell_integration_enabled),
        ),
        _ => {
            let shell = resolve_shell_path(settings);
            (shell, vec!["-l".to_string()])
        }
    }
}

fn claude_shell_kind(shell_program: &str) -> ClaudeShellKind {
    let executable = shell_program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(shell_program)
        .to_ascii_lowercase();
    if matches!(
        executable.as_str(),
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
    ) {
        ClaudeShellKind::PowerShell
    } else if matches!(executable.as_str(), "cmd" | "cmd.exe") {
        ClaudeShellKind::Cmd
    } else {
        ClaudeShellKind::Posix
    }
}

fn claude_hook_base_root() -> PathBuf {
    std::env::temp_dir().join("devmanager").join("claude-hooks")
}

fn prepare_claude_overlay_process_root() -> PathBuf {
    let base = claude_hook_base_root();
    let _ = std::fs::create_dir_all(&base);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700));
    }
    cleanup_orphaned_claude_overlay_roots_at(&base, |pid, started_at| {
        platform_service::process_matches_identity(pid, started_at, None)
    });

    let pid = std::process::id();
    let started_at = platform_service::capture_process_identity(pid)
        .map(|identity| identity.started_at_unix_secs)
        .unwrap_or(0);
    let token = claude_overlay_owner_token();
    base.join(format!("owner-{pid}-{started_at}-{token}"))
}

fn claude_overlay_owner_token() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_ok() {
        let mut encoded = String::with_capacity(32);
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(encoded, "{byte:02x}");
        }
        return encoded;
    }
    let counter = CLAUDE_OVERLAY_OWNER_COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:032x}", time ^ counter)
}

fn parse_claude_overlay_owner(path: &Path) -> Option<(u32, u64)> {
    let name = path.file_name()?.to_str()?.strip_prefix("owner-")?;
    let mut fields = name.split('-');
    let pid = fields.next()?.parse().ok()?;
    let started_at = fields.next()?.parse().ok()?;
    let token = fields.next()?;
    if fields.next().is_some()
        || token.len() != 32
        || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some((pid, started_at))
}

fn cleanup_orphaned_claude_overlay_roots_at(
    base: &Path,
    mut owner_is_alive: impl FnMut(u32, u64) -> bool,
) -> usize {
    let Ok(entries) = std::fs::read_dir(base) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some((pid, started_at)) = parse_claude_overlay_owner(&path) else {
            continue;
        };
        // A zero start time cannot distinguish PID reuse. Preserve it rather
        // than risking another live DevManager instance.
        if started_at == 0 || owner_is_alive(pid, started_at) {
            continue;
        }
        if remove_owned_claude_overlay_root(&path) {
            removed += 1;
        }
    }
    removed
}

fn remove_owned_claude_overlay_root(process_root: &Path) -> bool {
    let Some(base) = process_root.parent() else {
        return false;
    };
    let Ok(metadata) = std::fs::symlink_metadata(process_root) else {
        return false;
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return false;
    }
    let (Ok(canonical_base), Ok(canonical_root)) =
        (base.canonicalize(), process_root.canonicalize())
    else {
        return false;
    };
    if canonical_root.parent() != Some(canonical_base.as_path()) {
        return false;
    }
    std::fs::remove_dir_all(canonical_root).is_ok()
}

fn interactive_shell_command_from_inner(inner: &ProcessManagerInner) -> (String, Vec<String>) {
    let settings = inner
        .settings
        .read()
        .map(|settings| settings.clone())
        .unwrap_or_default();
    build_interactive_shell_command(&settings)
}

fn resolve_ai_startup_command(settings: &Settings, tab_type: TabType) -> Result<String, String> {
    let configured = match tab_type {
        TabType::Claude => settings
            .claude_command
            .clone()
            .unwrap_or_else(|| DEFAULT_CLAUDE_COMMAND.to_string()),
        TabType::Codex => settings
            .codex_command
            .clone()
            .unwrap_or_else(|| DEFAULT_CODEX_COMMAND.to_string()),
        _ => return Err("Unsupported AI tab type".to_string()),
    };

    let trimmed = configured.trim().to_string();
    if trimmed.is_empty() {
        Err("AI command is empty".to_string())
    } else {
        Ok(trimmed)
    }
}

fn default_ai_label(tab_type: TabType) -> String {
    match tab_type {
        TabType::Claude => "Claude".to_string(),
        TabType::Codex => "Codex".to_string(),
        _ => "AI".to_string(),
    }
}

fn resolve_shell_path(settings: &Settings) -> String {
    if cfg!(target_os = "macos") {
        match settings.mac_terminal_profile {
            Some(crate::models::MacTerminalProfile::Zsh) => "/bin/zsh".to_string(),
            Some(crate::models::MacTerminalProfile::Bash) => "/bin/bash".to_string(),
            _ => std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string()),
        }
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
    }
}

fn build_shell_command_line(command: &RunCommand) -> String {
    let mut parts = Vec::with_capacity(command.args.len() + 1);
    parts.push(command.command.trim().to_string());
    for arg in &command.args {
        parts.push(shell_quote(arg));
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn os_to_launch_string(
    value: &std::ffi::OsStr,
) -> Result<String, crate::providers::session::ProviderLaunchError> {
    value
        .to_str()
        .map(str::to_string)
        .ok_or(crate::providers::session::ProviderLaunchError::SpawnFailed)
}

fn issue_host_terminal_authority(
    inner: &ProcessManagerInner,
    session_id: &str,
    ports: impl IntoIterator<Item = u16>,
) -> Result<TerminalLaunchAuthority, String> {
    let mut bounded_ports = Vec::with_capacity(MAX_MANAGED_TERMINAL_PORTS);
    for port in ports {
        if bounded_ports.len() == MAX_MANAGED_TERMINAL_PORTS {
            return Err(format!(
                "terminal launch port set exceeds {MAX_MANAGED_TERMINAL_PORTS} entries"
            ));
        }
        bounded_ports.push(port);
    }
    inner
        .terminal_authority_issuer
        .issue(session_id, ProcessOwner::Host, bounded_ports)
}

fn spawn_server_session_with_inner(
    inner: &Arc<ProcessManagerInner>,
    launch: &ServerLaunchSpec,
    dimensions: SessionDimensions,
) -> Result<(), String> {
    let session_id = launch.command_id.clone();
    let session_live = inner
        .runtime_state
        .read()
        .ok()
        .and_then(|runtime| {
            runtime
                .sessions
                .get(&session_id)
                .map(|session| session.has_live_process())
        })
        .unwrap_or(false);
    let session_handle_exists = inner
        .sessions
        .lock()
        .ok()
        .map(|sessions| sessions.contains_key(&session_id))
        .unwrap_or(false);
    if session_live && session_handle_exists {
        return Ok(());
    }

    ensure_prior_session_teardown_settled(inner, &session_id, Duration::from_secs(2))?;

    if let Ok(existing_session) = inner
        .sessions
        .lock()
        .map(|sessions| sessions.get(&session_id).cloned())
    {
        if let Some(session) = existing_session {
            let authority =
                issue_host_terminal_authority(inner, &session_id, launch.port.into_iter())?;
            return session.restart_command(
                launch.cwd.clone(),
                dimensions,
                launch.program.clone(),
                launch.args.clone(),
                launch.env.clone(),
                launch.log_file_path.clone(),
                true,
                authority,
            );
        }
    }

    let authority = issue_host_terminal_authority(inner, &session_id, launch.port.into_iter())?;
    let session = TerminalSession::spawn_command(
        session_id.clone(),
        launch.cwd.clone(),
        dimensions,
        launch.program.clone(),
        launch.args.clone(),
        launch.env.clone(),
        inner
            .scrollback_lines
            .read()
            .map(|lines| *lines)
            .unwrap_or(10_000),
        launch.log_file_path.clone(),
        inner.runtime_state.clone(),
        inner.debug_enabled,
        Some(session_change_notifier(inner.clone(), session_id.clone())),
        Some(session_output_notifier(inner.clone(), session_id.clone())),
        authority,
    )?;

    if let Ok(mut sessions) = inner.sessions.lock() {
        sessions.insert(session_id.clone(), Arc::new(session));
    }

    let mut active_changed = false;
    if let Ok(mut runtime) = inner.runtime_state.write() {
        if runtime.active_session_id.is_none() {
            runtime.active_session_id = Some(session_id);
            active_changed = true;
        }
    }
    if active_changed {
        bump_runtime_revision(inner);
    }

    Ok(())
}

fn restore_interrupted_server_prompt(
    inner: &Arc<ProcessManagerInner>,
    session_id: &str,
    cwd: PathBuf,
    dimensions: SessionDimensions,
) -> Result<(), String> {
    let (shell_program, shell_args) = interactive_shell_command_from_inner(inner);
    let existing_session = inner
        .sessions
        .lock()
        .map_err(|_| "Session store poisoned".to_string())?
        .get(session_id)
        .cloned();

    if let Some(session) = existing_session {
        let authority = issue_host_terminal_authority(inner, session_id, Vec::new())?;
        session.restart_command(
            cwd.clone(),
            dimensions,
            shell_program.clone(),
            shell_args,
            HashMap::new(),
            None,
            false,
            authority,
        )?;
    } else {
        let authority = issue_host_terminal_authority(inner, session_id, Vec::new())?;
        let session = TerminalSession::spawn_command(
            session_id.to_string(),
            cwd.clone(),
            dimensions,
            shell_program.clone(),
            shell_args,
            HashMap::new(),
            inner
                .scrollback_lines
                .read()
                .map(|lines| *lines)
                .unwrap_or(10_000),
            None,
            inner.runtime_state.clone(),
            inner.debug_enabled,
            Some(session_change_notifier(
                inner.clone(),
                session_id.to_string(),
            )),
            Some(session_output_notifier(
                inner.clone(),
                session_id.to_string(),
            )),
            authority,
        )?;
        inner
            .sessions
            .lock()
            .map_err(|_| "Session store poisoned".to_string())?
            .insert(session_id.to_string(), Arc::new(session));
    }

    let mut changed = false;
    if let Ok(mut runtime) = inner.runtime_state.write() {
        if let Some(session) = runtime.sessions.get_mut(session_id) {
            let dirty_before = session.dirty_generation;
            session.cwd = cwd;
            session.dimensions = dimensions;
            session.activate_interactive_shell(
                shell_program,
                "Server interrupted with Ctrl+C. Terminal ready.",
            );
            changed = session.dirty_generation != dirty_before;
        }
    }
    if changed {
        bump_runtime_revision(inner);
        emit_tracked_remote_runtime_snapshot(inner, session_id);
    }

    Ok(())
}

fn mark_remote_session_dirty(inner: &ProcessManagerInner, session_id: &str) {
    if let Ok(mut dirty) = inner.remote_dirty_sessions.lock() {
        dirty.insert(session_id.to_string());
    }
}

fn bump_runtime_revision(inner: &ProcessManagerInner) {
    inner.runtime_revision.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn bump_server_lifecycle_generation(inner: &ProcessManagerInner) {
    inner
        .server_lifecycle_generation
        .fetch_add(1, Ordering::AcqRel);
}

fn current_runtime_generation(inner: &ProcessManagerInner, session_id: &str) -> Option<u64> {
    inner.runtime_state.read().ok().and_then(|runtime| {
        runtime
            .sessions
            .get(session_id)
            .map(|session| session.dirty_generation)
    })
}

fn remember_runtime_generation(inner: &ProcessManagerInner, session_id: &str, generation: u64) {
    if let Ok(mut observed) = inner.observed_runtime_generations.lock() {
        observed.insert(session_id.to_string(), generation);
    }
}

fn remember_current_runtime_generation(inner: &ProcessManagerInner, session_id: &str) {
    if let Some(generation) = current_runtime_generation(inner, session_id) {
        remember_runtime_generation(inner, session_id, generation);
    }
}

fn note_runtime_generation_change(inner: &ProcessManagerInner, session_id: &str) -> bool {
    let Some(generation) = current_runtime_generation(inner, session_id) else {
        return false;
    };
    let changed = inner
        .observed_runtime_generations
        .lock()
        .map(|mut observed| {
            if observed.get(session_id).copied() == Some(generation) {
                return false;
            }
            observed.insert(session_id.to_string(), generation);
            true
        })
        .unwrap_or(true);
    if changed {
        bump_runtime_revision(inner);
    }
    changed
}

fn emit_tracked_remote_runtime_snapshot(inner: &ProcessManagerInner, session_id: &str) {
    remember_current_runtime_generation(inner, session_id);
    emit_remote_runtime_snapshot(inner, session_id);
}

fn cleanup_claude_hook_session_if_matches(
    inner: &ProcessManagerInner,
    session_id: &str,
    expected: &ClaudeHookRegistration,
) -> bool {
    fence_and_remove_claude_hook_session(inner, session_id, Some(expected)).is_some()
}

fn emit_codex_semantic_if_current(
    inner: &ProcessManagerInner,
    session_id: &str,
    identity: &CodexAdapterIdentity,
    draft: SemanticEventDraft,
) {
    let mut registry = inner
        .codex_adapter_registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !registry.is_current(identity) {
        return;
    }
    let Some(session) = registry.sessions.get_mut(session_id) else {
        return;
    };
    if session.identity() == identity {
        emit_remote_session_event(
            inner,
            RemoteSessionEvent::CodexSemantic {
                identity: codex_semantic_identity(session_id, identity),
                draft,
            },
        );
    }
}

fn emit_codex_health_if_current(
    inner: &ProcessManagerInner,
    identity: &CodexAdapterIdentity,
    health: SemanticAdapterHealth,
) {
    let is_current = inner
        .codex_adapter_registry
        .lock()
        .map(|registry| registry.is_current(identity))
        .unwrap_or(false);
    if is_current {
        emit_remote_session_event(
            inner,
            RemoteSessionEvent::AdapterHealth {
                stable_session_key: identity.stable_session_key.clone(),
                health,
            },
        );
    }
}

fn handle_codex_hook_registry_event(
    inner: &Arc<ProcessManagerInner>,
    registration: CodexHookRegistration,
    event: CodexRegistryEvent,
) {
    let located = {
        let registry = inner
            .codex_adapter_registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry
            .sessions
            .iter()
            .find_map(|(session_id, session)| match session {
                CodexAdapterSession::Running {
                    identity,
                    registration: current,
                    exact_resume,
                    ..
                } if current.nonce == registration.nonce => {
                    Some((session_id.clone(), identity.clone(), exact_resume.clone()))
                }
                _ => None,
            })
    };
    let Some((session_id, identity, exact_resume)) = located else {
        return;
    };
    match event {
        CodexRegistryEvent::Semantic(draft) => {
            emit_codex_semantic_if_current(inner, &session_id, &identity, draft);
        }
        CodexRegistryEvent::SessionStarted(binding) => {
            handle_codex_session_started(inner, &session_id, &identity, binding);
        }
        CodexRegistryEvent::ExactResumeFailed => {
            handle_codex_exact_resume_failed(
                inner,
                &session_id,
                &identity,
                &registration,
                exact_resume.as_ref(),
            );
        }
    }
}

fn handle_codex_exact_resume_failed(
    inner: &Arc<ProcessManagerInner>,
    session_id: &str,
    identity: &CodexAdapterIdentity,
    registration: &CodexHookRegistration,
    exact_resume: Option<&CodexExactResumeLaunchBinding>,
) {
    // The adapter entry may already have been replaced after the event was
    // located. Retire the event's exact relay capability independently; the
    // identity-fenced cleanup below must never remove the replacement.
    inner.codex_hook_registry.unregister(&registration.nonce);
    emit_codex_health_if_current(inner, identity, SemanticAdapterHealth::Degraded);
    if !cleanup_codex_adapter_session_if_matches(inner, session_id, identity) {
        return;
    }

    let live = exact_resume.and_then(|binding| {
        inner.provider_runtime.lock().ok().and_then(|mut book| {
            let live = book
                .live
                .get(&binding.key())
                .filter(|live| live.session_id == session_id && binding.matches_live(live))
                .cloned();
            if live.is_none() {
                book.latched_exact_resume_failures
                    .insert(binding.key(), binding.clone());
            }
            live
        })
    });

    let settlement_error = live.as_ref().and_then(|live| {
        if arm_provider_abort(inner, live) {
            queue_provider_session_failure(
                inner,
                ProviderSessionFailure {
                    task_id: live.task_id,
                    agent_session_id: live.agent_session_id,
                    provider_kind: live.provider_kind,
                    failure: crate::providers::session::ExactResumeFailure::ProviderRejected,
                },
            );
        }
        reconcile_one_provider_terminal_exit(inner, &live.session_id);
        inner.provider_runtime.lock().ok().and_then(|book| {
            book.live
                .values()
                .any(|current| current.correlation == live.correlation)
                .then_some("exact runtime teardown is armed for bounded retry".to_string())
        })
    });

    let mut changed = false;
    if let Ok(mut runtime) = inner.runtime_state.write() {
        if let Some(session) = runtime.sessions.get_mut(session_id) {
            session.status = SessionStatus::Failed;
            session.exit = Some(SessionExitState {
                code: None,
                signal: None,
                closed_by_user: false,
                summary: settlement_error.map_or_else(
                    || "Exact provider resume failed: ProviderRejected".to_string(),
                    |error| {
                        format!(
                            "Exact provider resume failed: ProviderRejected; teardown remains retryable: {error}"
                        )
                    },
                ),
            });
            session.mark_dirty();
            changed = true;
        }
    }
    if changed {
        bump_runtime_revision(inner);
        mark_remote_session_dirty(inner, session_id);
        emit_tracked_remote_runtime_snapshot(inner, session_id);
    }
}

fn handle_codex_session_started(
    inner: &Arc<ProcessManagerInner>,
    session_id: &str,
    identity: &CodexAdapterIdentity,
    binding: CodexSessionBinding,
) {
    bind_runtime_provider_session_id(inner, session_id, binding.session_id.as_str().to_owned());
    let newly_activated = {
        let mut registry = inner
            .codex_adapter_registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !registry.is_current(identity) {
            false
        } else {
            match registry.sessions.get_mut(session_id) {
                Some(CodexAdapterSession::Running {
                    identity: current,
                    activated,
                    ..
                }) if current == identity => {
                    let newly_activated = !*activated;
                    *activated = true;
                    newly_activated
                }
                _ => false,
            }
        }
    };
    if newly_activated {
        emit_remote_session_event(
            inner,
            RemoteSessionEvent::AdapterHealth {
                stable_session_key: identity.stable_session_key.clone(),
                health: SemanticAdapterHealth::Healthy,
            },
        );
    }
}

fn arm_provider_abort(inner: &ProcessManagerInner, live: &ProviderLiveSession) -> bool {
    inner
        .provider_runtime
        .lock()
        .map(|mut book| {
            let Some(current) = book.live.values_mut().find(|current| {
                current.session_id == live.session_id && current.correlation == live.correlation
            }) else {
                return !live.failure_reported;
            };
            current.provider_identity_confirmed = false;
            current.settlement_kind = ProviderSettlementKind::AbortRejectedSessionStart;
            current.settlement_failures = 0;
            current.next_settlement_attempt = None;
            current.exit_reported = false;
            let should_report = !current.failure_reported;
            current.failure_reported = true;
            should_report
        })
        .unwrap_or(!live.failure_reported)
}

fn fail_provider_session_start(
    inner: &ProcessManagerInner,
    live: &ProviderLiveSession,
    error: &str,
) {
    cleanup_ai_adapters_for_session_inner(inner, &live.session_id);
    let should_report = arm_provider_abort(inner, live);
    if should_report {
        queue_provider_session_failure(
            inner,
            ProviderSessionFailure {
                task_id: live.task_id,
                agent_session_id: live.agent_session_id,
                provider_kind: live.provider_kind,
                failure: crate::providers::session::ExactResumeFailure::ProviderRejected,
            },
        );
    }
    reconcile_one_provider_terminal_exit(inner, &live.session_id);

    let mut changed = false;
    if let Ok(mut runtime) = inner.runtime_state.write() {
        if let Some(session) = runtime.sessions.get_mut(&live.session_id) {
            session.status = SessionStatus::Failed;
            session.provider_session_id = None;
            session.exit = Some(SessionExitState {
                code: None,
                signal: None,
                closed_by_user: false,
                summary: "Provider conversation identity could not be persisted; the exact runtime was stopped and no fresh conversation was substituted."
                    .to_string(),
            });
            session.mark_dirty();
            changed = true;
        }
    }
    if changed {
        bump_runtime_revision(inner);
        mark_remote_session_dirty(inner, &live.session_id);
        emit_tracked_remote_runtime_snapshot(inner, &live.session_id);
    }
    eprintln!("provider SessionStart persistence failed; exact runtime rejected: {error}");
}

fn bind_runtime_provider_session_id(
    inner: &ProcessManagerInner,
    pty_session_id: &str,
    provider_session_id: String,
) {
    let Ok(provider_session_id) =
        crate::domain::ProviderSessionId::new(provider_session_id.clone())
    else {
        return;
    };
    bind_runtime_provider_session_id_inner(inner, pty_session_id, provider_session_id, true);
}

fn bind_runtime_provider_session_id_inner(
    inner: &ProcessManagerInner,
    pty_session_id: &str,
    provider_session_id: crate::domain::ProviderSessionId,
    allow_live_recheck: bool,
) {
    // SessionStart is write-once for a live PTY generation. The registry is
    // the primary fence, but keep this ProcessManager projection fail-closed
    // as well so a late adapter callback can never rebind a running session to
    // a different conversation identity.
    let (live, conflict) = {
        let mut book = inner
            .provider_runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match book
            .live
            .values()
            .find(|live| live.session_id == pty_session_id)
            .cloned()
        {
            None => (None, None),
            Some(current)
                if current
                    .provider_session_id
                    .as_ref()
                    .is_some_and(|bound| bound != &provider_session_id) =>
            {
                (None, Some(current))
            }
            Some(current)
                if current.provider_identity_confirmed
                    || current.provider_identity_acceptance_started =>
            {
                return;
            }
            Some(current) => {
                let claimed = book
                    .live
                    .values_mut()
                    .find(|live| {
                        live.session_id == pty_session_id && live.correlation == current.correlation
                    })
                    .expect("live provider claim remains present");
                claimed.provider_session_id = Some(provider_session_id.clone());
                claimed.provider_identity_acceptance_started = true;
                (Some(claimed.clone()), None)
            }
        }
    };
    if let Some(conflict) = conflict.as_ref() {
        fail_provider_session_start(
            inner,
            conflict,
            "authenticated SessionStart conflicts with the exact live provider identity",
        );
        return;
    }
    if let Some(live) = live.as_ref() {
        let acceptance = inner
            .provider_sessions
            .lock()
            .map_err(|_| "provider session manager lock poisoned".to_string())
            .and_then(|mut managers| {
                let manager = managers
                    .as_mut()
                    .ok_or_else(|| "provider session manager is unavailable".to_string())?;
                let provenance = crate::providers::session::ProviderSessionStartProvenance::from_authenticated_hook(
                    live.correlation,
                    provider_session_id.clone(),
                );
                manager
                    .accept_provider_session_start(provenance)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            });
        if let Err(error) = acceptance {
            fail_provider_session_start(inner, live, &error);
            return;
        }
    }
    let changed = {
        let Ok(mut runtime) = inner.runtime_state.write() else {
            return;
        };
        let Some(session) = runtime.sessions.get_mut(pty_session_id) else {
            return;
        };
        if session
            .provider_session_id
            .as_deref()
            .is_some_and(|bound| bound != provider_session_id.as_str())
        {
            return;
        }
        if session.provider_session_id.as_deref() == Some(provider_session_id.as_str()) {
            false
        } else {
            // `Display` intentionally redacts this opaque identity.  The
            // runtime projection still stores the exact provider-issued value
            // so resume never receives the redacted diagnostic label.
            session.provider_session_id = Some(provider_session_id.as_str().to_owned());
            session.mark_dirty();
            true
        }
    };
    if let (Some(accepted), Ok(mut book)) = (live.as_ref(), inner.provider_runtime.lock()) {
        if let Some(current) = book.live.values_mut().find(|current| {
            current.session_id == pty_session_id && current.correlation == accepted.correlation
        }) {
            current.provider_session_id = Some(provider_session_id.clone());
            current.provider_identity_confirmed = true;
            current.provider_identity_acceptance_started = false;
        }
    }
    if changed {
        bump_runtime_revision(inner);
        emit_tracked_remote_runtime_snapshot(inner, pty_session_id);
    }
    if live.is_none() && allow_live_recheck {
        bind_runtime_provider_session_id_inner(inner, pty_session_id, provider_session_id, false);
    }
}

fn reconcile_provider_session_start_after_launch(
    inner: &ProcessManagerInner,
    correlation: crate::providers::session::RuntimeCorrelation,
) {
    let session_id = {
        let book = inner
            .provider_runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        book.live
            .values()
            .find(|live| live.correlation == correlation && !live.provider_identity_confirmed)
            .map(|live| live.session_id.clone())
    };
    let Some(session_id) = session_id else {
        return;
    };
    let provider_session_id = inner.runtime_state.read().ok().and_then(|runtime| {
        runtime
            .sessions
            .get(&session_id)?
            .provider_session_id
            .clone()
    });
    if let Some(provider_session_id) = provider_session_id {
        bind_runtime_provider_session_id(inner, &session_id, provider_session_id);
    }
}

fn mark_codex_adapter_degraded(
    inner: &ProcessManagerInner,
    session_id: &str,
    identity: &CodexAdapterIdentity,
) {
    let mut registry = inner
        .codex_adapter_registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !registry.is_current(identity)
        || !registry
            .sessions
            .get(session_id)
            .is_some_and(|session| session.identity() == identity)
    {
        return;
    }
    let previous = registry.sessions.insert(
        session_id.to_string(),
        CodexAdapterSession::Degraded(identity.clone()),
    );
    emit_remote_session_event(
        inner,
        RemoteSessionEvent::AdapterHealth {
            stable_session_key: identity.stable_session_key.clone(),
            health: SemanticAdapterHealth::Degraded,
        },
    );
    drop(registry);
    drop(previous);
}

fn cleanup_codex_adapter_session_if_matches(
    inner: &ProcessManagerInner,
    session_id: &str,
    expected: &CodexAdapterIdentity,
) -> bool {
    let removed = {
        let mut registry = inner
            .codex_adapter_registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let matches = registry
            .sessions
            .get(session_id)
            .is_some_and(|session| session.identity() == expected);
        matches
            .then(|| registry.remove_session(session_id))
            .flatten()
    };
    let was_removed = removed.is_some();
    let relay_nonce = removed.as_ref().and_then(|session| match session {
        CodexAdapterSession::Running { registration, .. } => Some(registration.nonce.clone()),
        CodexAdapterSession::Pending(_) | CodexAdapterSession::Degraded(_) => None,
    });
    let removed_identity = removed
        .as_ref()
        .and_then(|session| session.registered_semantic_identity(session_id));
    drop(removed);
    if let Some(nonce) = relay_nonce {
        inner.codex_hook_registry.unregister(&nonce);
    }
    if let Some(identity) = removed_identity {
        emit_remote_session_event(inner, RemoteSessionEvent::CodexAdapterRemoved { identity });
    }
    was_removed
}

fn cleanup_browser_provider_session_if_matches(
    inner: &ProcessManagerInner,
    session_id: &str,
    expected: &BrowserGatewayRegistration,
) -> bool {
    let removed = {
        let mut sessions = inner
            .browser_provider_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let matches = sessions.get(session_id).is_some_and(|session| {
            session.registration.process_session_id() == expected.process_session_id()
                && session.registration.workspace_key() == expected.workspace_key()
                && session.registration.access().bearer_token() == expected.access().bearer_token()
        });
        matches.then(|| sessions.remove(session_id)).flatten()
    };
    let Some(removed) = removed else {
        return false;
    };
    removed.registrar.revoke(&removed.registration);
    true
}

fn session_change_notifier(
    inner: Arc<ProcessManagerInner>,
    session_id: String,
) -> Arc<dyn Fn() + Send + Sync> {
    let attachment_binding = inner.browser_attachment_broker.binding(&session_id);
    session_change_notifier_with_attachment_binding(inner, session_id, attachment_binding)
}

fn session_change_notifier_with_attachment_binding(
    inner: Arc<ProcessManagerInner>,
    session_id: String,
    attachment_binding: Option<BrowserAttachmentSessionBinding>,
) -> Arc<dyn Fn() + Send + Sync> {
    let claude_registration = inner
        .claude_hook_sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&session_id)
        .map(|session| session.registration.clone());
    let codex_identity = inner
        .codex_adapter_registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .sessions
        .get(&session_id)
        .map(|session| session.identity().clone());
    let browser_registration = inner
        .browser_provider_sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&session_id)
        .map(|session| session.registration.clone());
    let inner = Arc::downgrade(&inner);
    Arc::new(move || {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        if note_runtime_generation_change(&inner, &session_id) {
            mark_remote_session_dirty(&inner, &session_id);
            emit_remote_runtime_snapshot(&inner, &session_id);
        }
        let terminal_exited = inner
            .runtime_state
            .read()
            .ok()
            .and_then(|runtime| {
                runtime
                    .sessions
                    .get(&session_id)
                    .map(|session| !session.status.is_live())
            })
            .unwrap_or(true);
        if terminal_exited {
            reconcile_one_provider_terminal_exit(&inner, &session_id);
            unbind_attachment_if_matches(&inner, attachment_binding.as_ref());
            if let Some(registration) = claude_registration.as_ref() {
                cleanup_claude_hook_session_if_matches(&inner, &session_id, registration);
            }
            if let Some(identity) = codex_identity.as_ref() {
                cleanup_codex_adapter_session_if_matches(&inner, &session_id, identity);
            }
            if let Some(registration) = browser_registration.as_ref() {
                cleanup_browser_provider_session_if_matches(&inner, &session_id, registration);
            }
        }
    })
}

fn session_output_notifier(
    inner: Arc<ProcessManagerInner>,
    session_id: String,
) -> Arc<dyn Fn(Vec<u8>, TerminalModeSnapshot) + Send + Sync> {
    let inner = Arc::downgrade(&inner);
    Arc::new(move |bytes, mode| {
        if bytes.is_empty() {
            return;
        }
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let screen = ai_session_screen_snapshot(&inner, &session_id);
        emit_remote_session_event(
            &inner,
            RemoteSessionEvent::Output {
                session_id: session_id.clone(),
                bytes,
                mode,
                screen,
            },
        );
    })
}

fn ai_session_screen_snapshot(
    inner: &ProcessManagerInner,
    session_id: &str,
) -> Option<TerminalScreenSnapshot> {
    let is_ai = inner
        .runtime_state
        .read()
        .ok()
        .and_then(|runtime| {
            runtime
                .sessions
                .get(session_id)
                .map(|session| session.session_kind)
        })
        .is_some_and(|kind| matches!(kind, SessionKind::Claude | SessionKind::Codex));
    if !is_ai {
        return None;
    }
    let session = inner
        .sessions
        .lock()
        .ok()
        .and_then(|sessions| sessions.get(session_id).cloned())?;
    Some(session.snapshot())
}

fn emit_remote_session_event(inner: &ProcessManagerInner, event: RemoteSessionEvent) {
    let handler = inner
        .remote_session_handler
        .read()
        .ok()
        .and_then(|handler| handler.clone());
    if let Some(handler) = handler {
        handler(event);
    }
}

fn emit_remote_runtime_snapshot(inner: &ProcessManagerInner, session_id: &str) {
    let runtime = inner
        .runtime_state
        .read()
        .ok()
        .and_then(|runtime| runtime.sessions.get(session_id).cloned());
    let Some(runtime) = runtime else {
        return;
    };
    emit_remote_session_event(
        inner,
        RemoteSessionEvent::Runtime {
            session_id: session_id.to_string(),
            runtime,
        },
    );
}

fn emit_remote_session_removed(inner: &ProcessManagerInner, session_id: &str) {
    emit_remote_session_event(
        inner,
        RemoteSessionEvent::Removed {
            session_id: session_id.to_string(),
        },
    );
}

fn next_ai_session_id(tab_type: &TabType) -> String {
    let prefix = match tab_type {
        TabType::Claude => "claude",
        TabType::Codex => "codex",
        _ => "ai",
    };
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let counter = AI_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let scope = crate::persistence::runtime_session_scope();
    format!("{prefix}-{scope}-{millis:x}-{counter:x}")
}

fn next_ssh_session_id(connection_id: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let counter = SSH_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let scope = crate::persistence::runtime_session_scope();
    format!("{connection_id}-{scope}-{millis:x}-{counter:x}")
}

fn process_manager_from_inner(inner: Arc<ProcessManagerInner>) -> Result<ProcessManager, String> {
    process_manager_from_inner_core(inner)
}

fn process_manager_from_inner_core(
    inner: Arc<ProcessManagerInner>,
) -> Result<ProcessManager, String> {
    let op_queue = inner
        .op_queue
        .lock()
        .ok()
        .and_then(|queue| queue.upgrade())
        .ok_or_else(|| "Process operation queue is unavailable.".to_string())?;
    let claude_overlay_owner = inner
        .claude_overlay_owner
        .lock()
        .ok()
        .and_then(|owner| owner.upgrade())
        .ok_or_else(|| "Claude overlay owner is unavailable.".to_string())?;
    let handle_lifecycle = inner.handle_lifecycle.clone();
    Ok(ProcessManager {
        inner,
        op_queue,
        _claude_overlay_owner: claude_overlay_owner,
        handle_lifecycle,
        shutdown_vote: false,
    })
}

pub(crate) fn execute_process_op_inner(
    inner: &Arc<ProcessManagerInner>,
    op: ProcessOp,
) -> ProcessOpCompletion {
    let op_id = op.op_id();
    let target_id = op.target_id();
    let manager = match process_manager_from_inner(inner.clone()) {
        Ok(manager) => manager,
        Err(error) => return op.into_failure_completion(error),
    };
    let (kind, result, context, remote_response) = match op {
        ProcessOp::StartServer {
            launch,
            dimensions,
            activate,
            response,
            ..
        } => {
            if activate {
                manager.set_active_session(launch.command_id.clone());
            }
            let result = run_server_launch_with_port_admission(inner, &manager, &launch, || {
                #[cfg(test)]
                {
                    let spawner = inner
                        .server_session_spawner_test_hook
                        .read()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    if let Some(spawner) = spawner {
                        spawner(inner, &launch, dimensions)
                    } else {
                        spawn_server_session_with_inner(inner, &launch, dimensions)
                    }
                }
                #[cfg(not(test))]
                {
                    spawn_server_session_with_inner(inner, &launch, dimensions)
                }
            })
            .map_err(|error| {
                manager.update_session_state(&launch.command_id, |state| {
                    state.status = SessionStatus::Failed;
                    state.exit = Some(SessionExitState {
                        code: None,
                        signal: None,
                        closed_by_user: false,
                        summary: error.clone(),
                    });
                    state.mark_dirty();
                });
                error
            });
            if result.is_ok() {
                manager.update_session_state(&launch.command_id, |state| {
                    state.configure_server(launch.clone());
                });
            }
            (
                ProcessOpKind::StartServer,
                result.map(|_| ()),
                ProcessOpContext {
                    session_id: Some(launch.command_id.clone()),
                    focus: activate,
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::StopServer {
            command_id,
            wait,
            response,
            ..
        } => {
            let result = if wait.is_zero() {
                manager.stop_server(&command_id).map(|_| ())
            } else {
                if manager.stop_server_and_wait(&command_id, wait) {
                    Ok(())
                } else {
                    Err(format!("Failed to stop `{command_id}` cleanly."))
                }
            };
            (
                ProcessOpKind::StopServer,
                result,
                ProcessOpContext {
                    session_id: Some(command_id.clone()),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::RestartServer {
            launch,
            dimensions,
            banner,
            clear_logs,
            response,
            ..
        } => {
            let command_id = launch.command_id.clone();
            let result = (|| {
                let retained_output = if clear_logs {
                    None
                } else {
                    manager
                        .get_session(&command_id)
                        .ok()
                        .map(|session| restart_history_text(&session.snapshot()))
                };
                if !manager.stop_server_and_wait(&command_id, Duration::from_secs(5)) {
                    return Err(format!(
                        "Managed process `{command_id}` did not stop cleanly."
                    ));
                }
                manager.set_active_session(command_id.clone());
                // A restart always creates a fresh terminal process owner.
                // The old session has already reached ACTIVE_PROCESS_ZERO,
                // joined its actors, released its exact registry fence, and
                // dropped before this new authority is minted. Keep the port
                // reservation through that exact launch boundary.
                run_server_launch_with_port_admission(inner, &manager, &launch, || {
                    spawn_server_session_with_inner(inner, &launch, dimensions)?;
                    if let Some(retained_output) = retained_output.filter(|text| !text.is_empty()) {
                        manager.write_virtual_text(&command_id, &retained_output)?;
                        manager.write_virtual_text(&command_id, "\r\n")?;
                    }
                    let _ = manager
                        .write_virtual_text(&command_id, &format!("\x1b[33m{banner}\x1b[0m\r\n"));
                    manager.update_session_state(&command_id, |state| {
                        state.configure_server(launch.clone());
                    });
                    Ok(())
                })
            })();
            (
                ProcessOpKind::RestartServer,
                result,
                ProcessOpContext {
                    session_id: Some(command_id),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::StartSsh {
            launch,
            session_id,
            dimensions,
            key_warning,
            response,
            ..
        } => {
            let result = spawn_ssh_session_with_inner(inner, &launch, &session_id, dimensions);
            if let Some(error) = key_warning {
                let _ = manager.write_virtual_text(
                    &session_id,
                    &format!(
                        "[devmanager] Couldn't prepare the saved SSH key ({error}); trying password/agent auth instead.\r\n"
                    ),
                );
            }
            (
                ProcessOpKind::StartSsh,
                result,
                ProcessOpContext {
                    session_id: Some(session_id),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::RestartSsh {
            close_session_id,
            launch,
            session_id,
            dimensions,
            key_warning,
            response,
            ..
        } => {
            let result = (|| {
                if let Some(close_id) = close_session_id {
                    manager.close_session(&close_id)?;
                }
                spawn_ssh_session_with_inner(inner, &launch, &session_id, dimensions)
            })();
            if let Some(error) = key_warning {
                let _ = manager.write_virtual_text(
                    &session_id,
                    &format!(
                        "[devmanager] Couldn't prepare the saved SSH key ({error}); trying password/agent auth instead.\r\n"
                    ),
                );
            }
            (
                ProcessOpKind::RestartSsh,
                result,
                ProcessOpContext {
                    session_id: Some(session_id),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::CloseSsh {
            session_id,
            response,
            ..
        } => {
            let result = if let Some(session_id) = session_id {
                manager.close_session(&session_id)
            } else {
                Ok(())
            };
            (
                ProcessOpKind::CloseSsh,
                result,
                ProcessOpContext::default(),
                response,
            )
        }
        ProcessOp::SpawnAi {
            launch,
            session_id,
            dimensions,
            attachment_binding,
            response,
            ..
        } => {
            let result = spawn_ai_session_with_attachment_binding(
                inner,
                &launch,
                &session_id,
                dimensions,
                attachment_binding,
            );
            (
                ProcessOpKind::SpawnAi,
                result,
                ProcessOpContext {
                    session_id: Some(session_id),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::RestartAi {
            close_session_id,
            launch,
            session_id,
            dimensions,
            attachment_binding,
            response,
            ..
        } => {
            let result = (|| {
                if let Some(close_id) = close_session_id {
                    manager.close_session(&close_id)?;
                }
                spawn_ai_session_with_attachment_binding(
                    inner,
                    &launch,
                    &session_id,
                    dimensions,
                    attachment_binding,
                )
            })();
            (
                ProcessOpKind::RestartAi,
                result,
                ProcessOpContext {
                    session_id: Some(session_id),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::CloseAi {
            session_id,
            response,
            ..
        } => {
            let result = manager.close_session(&session_id);
            (
                ProcessOpKind::CloseAi,
                result,
                ProcessOpContext {
                    session_id: Some(session_id),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::StopAll {
            command_ids,
            wait,
            response,
            ..
        } => {
            let mut failures = Vec::new();
            for command_id in &command_ids {
                if wait.is_zero() {
                    if let Err(error) = manager.stop_server(command_id) {
                        failures.push(error);
                    }
                } else if !manager.stop_server_and_wait(command_id, wait) {
                    failures.push(format!("Failed to stop `{command_id}` cleanly."));
                }
            }
            let result = if failures.is_empty() {
                Ok(())
            } else {
                Err(failures.join(" "))
            };
            (
                ProcessOpKind::StopAll,
                result,
                ProcessOpContext::default(),
                response,
            )
        }
        ProcessOp::Shutdown { timeout, .. } => {
            let report = shutdown_managed_processes_inner(inner, timeout);
            (
                ProcessOpKind::Shutdown,
                if report.remaining_live_sessions == 0 && report.remaining_tracked_pids == 0 {
                    Ok(())
                } else {
                    Err(format!(
                        "Shutdown left {} live session(s) and {} tracked pid(s).",
                        report.remaining_live_sessions, report.remaining_tracked_pids
                    ))
                },
                ProcessOpContext {
                    shutdown_report: Some(report),
                    ..Default::default()
                },
                None,
            )
        }
        ProcessOp::KillProcess {
            session_id,
            pid,
            fence,
            response,
            ..
        } => {
            let result = close_managed_process_exact(inner, &session_id, &fence, pid, false);
            (
                ProcessOpKind::KillProcess,
                result,
                ProcessOpContext {
                    session_id: Some(session_id),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::KillProcessTree {
            session_id,
            pid,
            fence,
            response,
            ..
        } => {
            let result = close_managed_process_exact(inner, &session_id, &fence, pid, true);
            (
                ProcessOpKind::KillProcessTree,
                result,
                ProcessOpContext {
                    session_id: Some(session_id),
                    ..Default::default()
                },
                response,
            )
        }
    };

    ProcessOpCompletion {
        op_id,
        kind,
        target_id,
        result,
        context,
        remote_response,
    }
}

/// Keep the port reservation alive around the actual worker-side spawn or
/// restart call. A reservation acquired by a UI callback is not sufficient:
/// queueing only transfers intent, while this seam owns admission until the
/// operation has returned success or failure.
fn run_server_launch_with_port_admission<T>(
    inner: &Arc<ProcessManagerInner>,
    manager: &ProcessManager,
    launch: &ServerLaunchSpec,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let Some(port) = launch.port else {
        return operation();
    };

    let snapshot = inner
        .port_inventory
        .refresh(&[port])
        .map_err(|error| format!("could not establish whether port {port} is free: {error}"))?;
    let reservation = inner
        .port_inventory
        .reserve_start(&snapshot, port)
        .map_err(|error| error.to_string())?;
    let launch_result = crate::process::ports::launch_if_port_free_with_revalidation(
        &snapshot,
        port,
        || {
            if !reservation.is_active() {
                Err(crate::process::ports::PortStartError::ReservationConflict { port })
            } else {
                let second = inner.port_inventory.refresh(&[port]).map_err(|error| {
                    crate::process::ports::PortStartError::ProbeFailed {
                        port,
                        detail: error.to_string(),
                    }
                })?;
                crate::process::ports::ensure_managed_start_allowed(&second, port)
            }
        },
        operation,
    )
    .map_err(|error| error.to_string())?;

    let value = launch_result?;
    settle_server_port_start(inner, manager, launch, reservation)?;
    Ok(value)
}

const PORT_BIND_SETTLEMENT_TIMEOUT: Duration = Duration::from_millis(250);
const PORT_BIND_SETTLEMENT_POLL: Duration = Duration::from_millis(25);

/// A logical reservation cannot make an arbitrary child bind atomic. Keep the
/// reservation through the real spawn and a bounded post-launch reconciliation.
/// A proven foreign listener is reported as EADDRINUSE-like failure; no
/// unverified or foreign PID is ever terminated.
fn settle_server_port_start(
    inner: &Arc<ProcessManagerInner>,
    manager: &ProcessManager,
    launch: &ServerLaunchSpec,
    reservation: PortStartReservation,
) -> Result<(), String> {
    let Some(port) = launch.port else {
        drop(reservation);
        return Ok(());
    };
    let deadline = Instant::now()
        .checked_add(PORT_BIND_SETTLEMENT_TIMEOUT)
        .unwrap_or_else(Instant::now);

    loop {
        let snapshot = match inner.port_inventory.refresh(&[port]) {
            Ok(snapshot) => snapshot,
            Err(_error) if Instant::now() < deadline => {
                thread::sleep(PORT_BIND_SETTLEMENT_POLL);
                continue;
            }
            Err(error) => {
                let root_fence = launch_root_fence_description(inner, &launch.command_id);
                let reaped =
                    manager.stop_server_and_wait(&launch.command_id, Duration::from_secs(2));
                drop(reservation);
                return Err(if reaped {
                    format!(
                        "port {port} listener settlement probe failed; exact launch root {root_fence} was reaped (diagnostic {error})"
                    )
                } else {
                    format!(
                        "port {port} listener settlement probe failed; exact launch root {root_fence} reap_incomplete"
                    )
                });
            }
        };
        let listeners = snapshot
            .observation(port)
            .map(|observation| observation.listeners())
            .unwrap_or(&[]);
        #[cfg(windows)]
        let settlement =
            match current_job_members_for_port_settlement(inner, &launch.command_id, deadline) {
                Ok(Some(job_members)) => classify_post_launch_settlement_with_job_authority(
                    listeners,
                    Ok(job_members.as_slice()),
                    true,
                ),
                Ok(None) => classify_post_launch_settlement_with_job_authority(
                    listeners,
                    Err("job_authority_unavailable"),
                    true,
                ),
                Err(error) => classify_post_launch_settlement_with_job_authority(
                    listeners,
                    Err(error.as_str()),
                    true,
                ),
            };
        #[cfg(not(windows))]
        let settlement = Ok(classify_post_launch_listener_settlement(
            listeners,
            |listener| listener_matches_session(inner, &launch.command_id, listener),
        ));

        let settlement = match settlement {
            Ok(settlement) => settlement,
            Err(error) => {
                let root_fence = launch_root_fence_description(inner, &launch.command_id);
                let reaped =
                    manager.stop_server_and_wait(&launch.command_id, Duration::from_secs(2));
                drop(reservation);
                return Err(if reaped {
                    format!(
                        "port {port} listener settlement probe failed; exact launch root {root_fence} was reaped (diagnostic {error})"
                    )
                } else {
                    format!(
                        "port {port} listener settlement probe failed; exact launch root {root_fence} reap_incomplete"
                    )
                });
            }
        };

        match settlement {
            PostLaunchListenerSettlement::Owned => {
                drop(reservation);
                return Ok(());
            }
            PostLaunchListenerSettlement::Foreign => {
                // Stop only the exact launched session. The foreign listener
                // itself is never selected by this operation.
                let root_fence = launch_root_fence_description(inner, &launch.command_id);
                let reaped =
                    manager.stop_server_and_wait(&launch.command_id, Duration::from_secs(2));
                drop(reservation);
                return Err(if reaped {
                    format!(
                        "port {port} became occupied by a foreign listener during start (EADDRINUSE); no foreign process was terminated; exact launch root {root_fence} was reaped"
                    )
                } else {
                    format!(
                        "port {port} became occupied by a foreign listener during start (EADDRINUSE); no foreign process was terminated; exact launch root {root_fence} reap_incomplete"
                    )
                });
            }
            PostLaunchListenerSettlement::Unverified => {
                // The listener identity is insufficient to decide whether
                // the process owns the port. Reap only the exact launch
                // session so an unrelated listener is never touched.
                let root_fence = launch_root_fence_description(inner, &launch.command_id);
                let reaped =
                    manager.stop_server_and_wait(&launch.command_id, Duration::from_secs(2));
                drop(reservation);
                return Err(if reaped {
                    format!(
                        "port {port} listener ownership could not be proven after start; exact launch root {root_fence} was reaped"
                    )
                } else {
                    format!(
                        "port {port} listener ownership could not be proven after start; exact launch root {root_fence} reap_incomplete"
                    )
                });
            }
            PostLaunchListenerSettlement::Pending => {}
        }
        if Instant::now() >= deadline {
            // A stock command may spawn successfully and bind later, but a
            // successful operation must not hide an unsettled launch. Reap
            // only the exact launch session and surface settlement failure.
            let root_fence = launch_root_fence_description(inner, &launch.command_id);
            let reaped = manager.stop_server_and_wait(&launch.command_id, Duration::from_secs(2));
            drop(reservation);
            return Err(if reaped {
                format!(
                    "port {port} listener settlement timed out; exact launch root {root_fence} was reaped"
                )
            } else {
                format!(
                    "port {port} listener settlement timed out; exact launch root {root_fence} reap_incomplete"
                )
            });
        }
        thread::sleep(PORT_BIND_SETTLEMENT_POLL);
    }
}

fn launch_root_fence_description(inner: &Arc<ProcessManagerInner>, session_id: &str) -> String {
    let Some(pid) = live_runtime_root_pid(inner, session_id) else {
        return format!("session {session_id} root unavailable");
    };
    let creation = platform_service::capture_process_creation_time_100ns(pid)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    format!("session {session_id}, pid {pid}, creation {creation}, canonical executable bound")
}

/// Return the current runtime root only when the projection still carries the
/// exact managed-process fence that named that root. A runtime PID by itself is
/// diagnostic data and cannot establish ownership across a replacement.
fn live_runtime_root_pid(inner: &Arc<ProcessManagerInner>, session_id: &str) -> Option<u32> {
    let runtime = inner.runtime_state.read().ok()?;
    let session = runtime.sessions.get(session_id)?;
    let fence = session.resources.managed_process_fence.as_ref()?;
    let root_pid = fence.root().id().pid();
    (session.status.is_live()
        && session.pid == Some(root_pid)
        && session.resources.process_ids.contains(&root_pid))
    .then_some(root_pid)
}

/// Capture the exact current teardown-owned Job membership for one settlement
/// attempt. The background resource projection is intentionally not consulted:
/// it is allowed to be one sampler tick behind the child that just bound the
/// port. The TerminalSession API retains the Job handle and returns an identity
/// snapshot tied to its current generation, without exposing termination
/// authority or accepting a raw PID as proof.
#[cfg(windows)]
fn current_job_members_for_port_settlement(
    inner: &Arc<ProcessManagerInner>,
    session_id: &str,
    deadline: Instant,
) -> Result<Option<Vec<JobMemberObservation>>, String> {
    if Instant::now() >= deadline {
        return Err("settlement observation exceeded deadline".to_string());
    }

    let session = inner
        .sessions
        .lock()
        .map_err(|_| "Session store poisoned".to_string())?
        .get(session_id)
        .cloned();
    let Some(session) = session else {
        return Ok(None);
    };

    let Some(observation) = session
        .managed_process_observations_until(deadline, RESOURCE_SAMPLE_MAX_MEMBERS_PER_TICK)?
    else {
        return Ok(None);
    };
    let (capture, members) = observation.into_parts();
    let members = members?;

    // The Job query captures the fence before inspecting members. Re-read the
    // fence and map entry before admitting the result so a concurrent restart
    // cannot turn an old generation's membership into current ownership.
    let current_fence = session
        .managed_process_fence()?
        .ok_or_else(|| "settlement_generation_stale".to_string())?;
    if current_fence != *capture.fence() {
        return Err("settlement_generation_stale".to_string());
    }
    let current_session = inner
        .sessions
        .lock()
        .map_err(|_| "Session store poisoned".to_string())?
        .get(session_id)
        .cloned();
    if !current_session.is_some_and(|current| Arc::ptr_eq(&current, &session)) {
        return Err("settlement_generation_stale".to_string());
    }

    Ok(Some(members))
}

/// Return only the process IDs from the same current, fenced resource sample
/// as the runtime root. This is a read-only ownership projection; it never
/// reconstructs control authority from a PID or from the persistence ledger.
fn session_managed_process_ids(inner: &ProcessManagerInner, session_id: &str) -> Vec<u32> {
    let Ok(runtime) = inner.runtime_state.read() else {
        return Vec::new();
    };
    let Some(session) = runtime.sessions.get(session_id) else {
        return Vec::new();
    };
    let Some(fence) = session.resources.managed_process_fence.as_ref() else {
        return Vec::new();
    };
    let root_pid = fence.root().id().pid();
    if !session.status.is_live()
        || session.pid != Some(root_pid)
        || !session.resources.process_ids.contains(&root_pid)
    {
        return Vec::new();
    }
    session.resources.process_ids.clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostLaunchListenerSettlement {
    /// No listener is visible yet; keep polling while the reservation remains
    /// owned by the start operation.
    Pending,
    /// Every observed listener was proven to belong to the exact launched
    /// session generation.
    Owned,
    /// At least one listener has complete identity evidence and is not owned
    /// by the exact launched session. The caller may report EADDRINUSE, but it
    /// must never terminate that foreign process.
    Foreign,
    /// A listener exists but its ownership cannot be proven. Fail closed and
    /// stop only the exact launch session; no unrelated process is selected.
    Unverified,
}

fn classify_post_launch_listener_settlement<F>(
    listeners: &[crate::process::ports::ListenerIdentity],
    mut owns: F,
) -> PostLaunchListenerSettlement
where
    F: FnMut(&crate::process::ports::ListenerIdentity) -> bool,
{
    if listeners.is_empty() {
        return PostLaunchListenerSettlement::Pending;
    }
    let ownership = listeners
        .iter()
        .map(|listener| (listener.has_executable_proof(), owns(listener)))
        .collect::<Vec<_>>();
    if ownership.iter().all(|(_, owned)| *owned) {
        return PostLaunchListenerSettlement::Owned;
    }
    if ownership
        .iter()
        .any(|(executable_proven, owned)| *executable_proven && !*owned)
    {
        return PostLaunchListenerSettlement::Foreign;
    }
    PostLaunchListenerSettlement::Unverified
}

fn listener_owned_by_current_job_member(
    listener: &crate::process::ports::ListenerIdentity,
    job_members: &[JobMemberObservation],
) -> bool {
    let Some(listener_executable) = listener.canonical_executable() else {
        return false;
    };
    job_members.iter().any(|member| match member {
        JobMemberObservation::Accessible { identity } => {
            let identity_id = identity.id();
            identity_id.pid() == listener.pid()
                && identity_id.creation_time_100ns() == listener.creation_time_100ns()
                && identity.canonical_executable() == listener_executable
        }
        JobMemberObservation::Inaccessible { .. } => false,
    })
}

fn classify_post_launch_listener_settlement_against_job(
    listeners: &[crate::process::ports::ListenerIdentity],
    job_members: &[JobMemberObservation],
) -> PostLaunchListenerSettlement {
    if listeners.is_empty() {
        return PostLaunchListenerSettlement::Pending;
    }
    let mut saw_unverified = false;
    for listener in listeners {
        if listener_owned_by_current_job_member(listener, job_members) {
            continue;
        }
        if !listener.has_executable_proof()
            || job_members.iter().any(|member| match member {
                JobMemberObservation::Accessible { identity } => {
                    identity.id().pid() == listener.pid()
                }
                JobMemberObservation::Inaccessible { pid, .. } => *pid == listener.pid(),
            })
        {
            saw_unverified = true;
            continue;
        }
        return PostLaunchListenerSettlement::Foreign;
    }
    if saw_unverified {
        PostLaunchListenerSettlement::Unverified
    } else {
        PostLaunchListenerSettlement::Owned
    }
}

/// Resolve one listener snapshot against the exact current Job query. A query
/// failure is allowed to remain pending while no listener exists, but once a
/// listener is visible it is surfaced instead of falling back to stale runtime
/// process IDs. This keeps the ownership decision fail-closed without turning
/// the sampler's delayed projection into control authority.
fn classify_post_launch_settlement_with_job_authority(
    listeners: &[crate::process::ports::ListenerIdentity],
    job_members: Result<&[JobMemberObservation], &str>,
    generation_current: bool,
) -> Result<PostLaunchListenerSettlement, String> {
    if !generation_current {
        return Err("settlement_generation_stale".to_string());
    }
    match job_members {
        Ok(job_members) => Ok(classify_post_launch_listener_settlement_against_job(
            listeners,
            job_members,
        )),
        Err(_error) if listeners.is_empty() => Ok(PostLaunchListenerSettlement::Pending),
        Err(error) => Err(error.to_string()),
    }
}

fn listener_matches_session(
    inner: &Arc<ProcessManagerInner>,
    session_id: &str,
    listener: &crate::process::ports::ListenerIdentity,
) -> bool {
    if !listener.has_executable_proof() {
        return false;
    }
    let Some(creation_time_100ns) =
        platform_service::capture_process_creation_time_100ns(listener.pid())
    else {
        return false;
    };
    let Some(executable) = platform_service::capture_process_executable(listener.pid()) else {
        return false;
    };
    let Ok(executable) = std::fs::canonicalize(executable) else {
        return false;
    };
    let Some(listener_executable) = listener.canonical_executable() else {
        return false;
    };
    if creation_time_100ns != listener.creation_time_100ns() || executable != listener_executable {
        return false;
    }
    let tracked = session_managed_process_ids(inner, session_id);
    if tracked.contains(&listener.pid()) {
        return true;
    }
    live_runtime_root_pid(inner, session_id) == Some(listener.pid())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillProcessOutcome {
    Killed,
    AlreadyGone,
}

fn close_managed_process_exact(
    inner: &Arc<ProcessManagerInner>,
    session_id: &str,
    fence: &ManagedProcessFence,
    diagnostic_pid: u32,
    kill_tree: bool,
) -> Result<(), String> {
    // The selected PID and Kill/Kill-tree wording are diagnostic only. Exact
    // control always closes the whole teardown-owned Job generation.
    let _ = (diagnostic_pid, kill_tree);
    let session = match inner.sessions.lock() {
        Ok(sessions) => sessions.get(session_id).cloned(),
        Err(_) => {
            clear_unowned_managed_process_projection(inner, session_id, true);
            return Err("Session store poisoned".to_string());
        }
    };
    let Some(session) = session else {
        clear_unowned_managed_process_projection(inner, session_id, true);
        return Err(format!(
            "Exact managed teardown authority for session `{session_id}` is unavailable"
        ));
    };

    #[cfg(windows)]
    session.close_managed_process_exact(fence, true)?;
    #[cfg(not(windows))]
    return Err("Exact managed-process close is unavailable off Windows".to_string());

    let removed = {
        let mut sessions = inner
            .sessions
            .lock()
            .map_err(|_| "Session store poisoned".to_string())?;
        match sessions.get(session_id) {
            Some(current) if Arc::ptr_eq(current, &session) => sessions.remove(session_id),
            Some(_) => {
                return Err(format!(
                    "Session `{session_id}` changed generations before exact owner release"
                ))
            }
            None => None,
        }
    };
    drop(removed);
    drop(session);
    let _ = pid_file::prune_inactive_entries();
    mark_session_reaped(inner, session_id);
    Ok(())
}

fn clear_unowned_managed_process_projection(
    inner: &Arc<ProcessManagerInner>,
    session_id: &str,
    closed_by_user: bool,
) {
    let has_live_ledger_evidence =
        !pid_file::active_tracked_pids_for_session(session_id).is_empty();
    let mut changed = false;
    let mut runtime = match inner.runtime_state.write() {
        Ok(runtime) => runtime,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(session) = runtime.sessions.get_mut(session_id) {
        let dirty_before = session.dirty_generation;
        let preserves_settlement = session.status == SessionStatus::Stopped
            && !session.reap_incomplete
            && !has_live_ledger_evidence;
        session.pid = None;
        if preserves_settlement {
            session.resources = ResourceSnapshot::default();
        } else {
            session.status = SessionStatus::Failed;
            session.reap_incomplete = true;
            session.resources = ResourceSnapshot {
                metrics_unavailable: true,
                metrics_status: ProcessMetricStatus::Failed,
                metric_values: ResourceMetricValueState::Unavailable,
                cpu_value_state: ResourceMetricValueState::Unavailable,
                memory_value_state: ResourceMetricValueState::Unavailable,
                process_count_value_state: ResourceMetricValueState::Unavailable,
                metrics_error: Some("exact_owner_unavailable".to_string()),
                last_sample_at: Some(Instant::now()),
                ..ResourceSnapshot::default()
            };
            session.exit = Some(SessionExitState {
                code: None,
                signal: None,
                closed_by_user,
                summary: "Exact managed teardown unavailable: terminal owner is missing"
                    .to_string(),
            });
        }
        session.mark_dirty();
        changed = session.dirty_generation != dirty_before;
    }
    drop(runtime);
    if changed {
        bump_runtime_revision(inner);
        mark_remote_session_dirty(inner, session_id);
        emit_tracked_remote_runtime_snapshot(inner, session_id);
    }
}

fn validate_process_op_host_string(value: &str, field: &str) -> Result<(), String> {
    if value.len() > MAX_PROCESS_OP_HOST_STRING_BYTES {
        return Err(format!(
            "{field} exceeds {MAX_PROCESS_OP_HOST_STRING_BYTES} bytes"
        ));
    }
    Ok(())
}

fn spawn_ssh_session_with_inner(
    inner: &Arc<ProcessManagerInner>,
    launch: &SshLaunchSpec,
    session_id: &str,
    dimensions: SessionDimensions,
) -> Result<(), String> {
    let manager = process_manager_from_inner(inner.clone())?;
    if manager.session_exists(session_id) {
        return Ok(());
    }
    ensure_prior_session_teardown_settled(inner, session_id, Duration::from_secs(2))?;
    let authority = issue_host_terminal_authority(inner, session_id, Vec::new())?;
    let session = TerminalSession::spawn_command(
        session_id.to_string(),
        launch.cwd.clone(),
        dimensions,
        launch.program.clone(),
        launch.args.clone(),
        HashMap::new(),
        inner
            .scrollback_lines
            .read()
            .map(|lines| *lines)
            .unwrap_or(10_000),
        None,
        inner.runtime_state.clone(),
        inner.debug_enabled,
        Some(session_change_notifier(
            inner.clone(),
            session_id.to_string(),
        )),
        Some(session_output_notifier(
            inner.clone(),
            session_id.to_string(),
        )),
        authority,
    )
    .map_err(|error| {
        manager.update_session_state(session_id, |state| {
            state.status = SessionStatus::Failed;
            state.exit = Some(SessionExitState {
                code: None,
                signal: None,
                closed_by_user: false,
                summary: error.clone(),
            });
            state.mark_dirty();
        });
        error
    })?;
    if let Ok(mut sessions) = inner.sessions.lock() {
        sessions.insert(session_id.to_string(), Arc::new(session));
    }
    Ok(())
}

#[cfg(test)]
fn spawn_ai_session_with_inner(
    inner: &Arc<ProcessManagerInner>,
    launch: &AiLaunchSpec,
    session_id: &str,
    dimensions: SessionDimensions,
) -> Result<(), String> {
    let attachment_binding = inner.browser_attachment_broker.binding(session_id);
    spawn_ai_session_with_attachment_binding(
        inner,
        launch,
        session_id,
        dimensions,
        attachment_binding,
    )
}

fn spawn_ai_session_with_attachment_binding(
    inner: &Arc<ProcessManagerInner>,
    launch: &AiLaunchSpec,
    session_id: &str,
    dimensions: SessionDimensions,
    attachment_binding: Option<BrowserAttachmentSessionBinding>,
) -> Result<(), String> {
    spawn_ai_session_with_writer_and_attachment_binding(
        inner,
        launch,
        session_id,
        dimensions,
        TerminalSession::write_text,
        attachment_binding,
    )
}

#[cfg(test)]
fn spawn_ai_session_with_writer<F>(
    inner: &Arc<ProcessManagerInner>,
    launch: &AiLaunchSpec,
    session_id: &str,
    dimensions: SessionDimensions,
    write_startup_command: F,
) -> Result<(), String>
where
    F: FnOnce(&TerminalSession, &str) -> Result<(), String>,
{
    let attachment_binding = inner.browser_attachment_broker.binding(session_id);
    spawn_ai_session_with_writer_and_attachment_binding(
        inner,
        launch,
        session_id,
        dimensions,
        write_startup_command,
        attachment_binding,
    )
}

fn spawn_ai_session_with_writer_and_attachment_binding<F>(
    inner: &Arc<ProcessManagerInner>,
    launch: &AiLaunchSpec,
    session_id: &str,
    dimensions: SessionDimensions,
    write_startup_command: F,
    attachment_binding: Option<BrowserAttachmentSessionBinding>,
) -> Result<(), String>
where
    F: FnOnce(&TerminalSession, &str) -> Result<(), String>,
{
    let manager = process_manager_from_inner(inner.clone())?;
    if manager.session_exists(session_id) {
        return Ok(());
    }
    ensure_prior_session_teardown_settled(inner, session_id, Duration::from_secs(2))?;
    let mut effective_launch = launch.clone();
    let terminal_env = manager.prepare_ai_terminal_environment(&mut effective_launch, session_id);
    manager.update_session_state(session_id, |state| {
        state.shell_program = effective_launch.shell_program.clone();
        state.configure_ai(effective_launch.clone());
    });
    let authority = issue_host_terminal_authority(inner, session_id, Vec::new())?;
    let session = TerminalSession::spawn_command(
        session_id.to_string(),
        effective_launch.cwd.clone(),
        dimensions,
        effective_launch.shell_program.clone(),
        effective_launch.shell_args.clone(),
        terminal_env,
        inner
            .scrollback_lines
            .read()
            .map(|lines| *lines)
            .unwrap_or(10_000),
        None,
        inner.runtime_state.clone(),
        inner.debug_enabled,
        Some(session_change_notifier_with_attachment_binding(
            inner.clone(),
            session_id.to_string(),
            attachment_binding.clone(),
        )),
        Some(session_output_notifier(
            inner.clone(),
            session_id.to_string(),
        )),
        authority,
    )
    .map_err(|error| {
        manager.cleanup_ai_adapters_for_session(session_id);
        unbind_attachment_if_matches(inner, attachment_binding.as_ref());
        manager.update_session_state(session_id, |state| {
            state.status = SessionStatus::Failed;
            state.exit = Some(SessionExitState {
                code: None,
                signal: None,
                closed_by_user: false,
                summary: error.clone(),
            });
            state.mark_dirty();
        });
        error
    })?;
    let session = Arc::new(session);
    if let Ok(mut sessions) = inner.sessions.lock() {
        sessions.insert(session_id.to_string(), session.clone());
    }
    // The PTY is already owned by the exact terminal authority before this
    // write. A fixed sleep here only delays the stock provider and can leave
    // the UI looking stalled; readiness is observed through the normal PTY
    // stream and provider hooks instead of a timing heuristic.
    let startup_command = effective_launch.startup_command + "\r\n";
    if let Err(write_error) = write_startup_command(&session, &startup_command) {
        let error = format!("inject AI startup command: {write_error}");
        manager.cleanup_ai_adapters_for_session(session_id);
        drop(session);
        if let Err(close_error) = manager.request_session_close(session_id, false) {
            return Err(format!(
                "{error}; exact managed teardown remains retryable: {close_error}"
            ));
        }
        unbind_attachment_if_matches(inner, attachment_binding.as_ref());
        manager.update_session_state(session_id, |state| {
            state.status = SessionStatus::Failed;
            state.exit = Some(SessionExitState {
                code: None,
                signal: None,
                closed_by_user: false,
                summary: error.clone(),
            });
            state.mark_dirty();
        });
        return Err(error);
    }
    Ok(())
}

fn shutdown_managed_processes_inner(
    inner: &Arc<ProcessManagerInner>,
    timeout: Duration,
) -> ManagedShutdownReport {
    let manager = process_manager_from_inner(inner.clone())
        .expect("managed shutdown requires an active ProcessManager handle");
    stop_background_workers_for_managed_shutdown(inner);
    let mut requested_sessions = 0usize;
    loop {
        let entry = {
            let mut sessions = inner
                .sessions
                .lock()
                .unwrap_or_else(|_| std::process::abort());
            let Some(session_id) = sessions.keys().next().cloned() else {
                break;
            };
            sessions
                .remove(&session_id)
                .map(|session| (session_id, session))
        };
        let Some((session_id, session)) = entry else {
            continue;
        };
        requested_sessions = requested_sessions.saturating_add(1);
        if let Err(error) = session.close(false) {
            // Retain the exact owner for a later retry. Publishing Stopped or
            // dropping this owner after a failed release would either lie
            // about settlement or trip the fail-closed Drop invariant.
            match inner.sessions.lock() {
                Ok(mut sessions) => match sessions.entry(session_id.clone()) {
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(session);
                    }
                    std::collections::hash_map::Entry::Occupied(_) => std::process::abort(),
                },
                Err(_) => std::process::abort(),
            }
            manager.note_exact_teardown_failure(&session_id, &error, false);
            manager.note_reap_incomplete(&session_id);
            break;
        }
        drop(session);
        // Reconcile only after the manager's final owner has dropped. The
        // close itself already proved zero, joined actors, released the exact
        // registry entry, and durably settled.
        manager.reconcile_closed_session(&session_id);
    }

    let started_at = Instant::now();
    let active_tracked_processes = loop {
        let _ = pid_file::prune_inactive_entries();
        let remaining_live_sessions = manager.live_session_count();
        let active_tracked_processes = pid_file::active_tracked_processes();
        if remaining_live_sessions == 0 && active_tracked_processes.is_empty() {
            break active_tracked_processes;
        }
        if started_at.elapsed() >= timeout {
            break active_tracked_processes;
        }
        thread::sleep(Duration::from_millis(100));
    };

    let _ = pid_file::prune_inactive_entries();
    let report = ManagedShutdownReport {
        requested_sessions,
        forced_kill_pids: 0,
        remaining_live_sessions: manager.live_session_count(),
        remaining_tracked_pids: active_tracked_processes.len(),
    };
    if report.remaining_live_sessions == 0 && report.remaining_tracked_pids == 0 {
        pid_file::clear_all();
    }
    manager.drain_claude_hook_adapter();
    manager.drain_browser_provider_adapter();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        AppConfig, Project, ProjectFolder, RunCommand, SessionTab, Settings, TabType,
    };

    #[test]
    fn pwsh_maps_to_resolved_path() {
        let (program, args) = windows_shell_for(
            &crate::models::DefaultTerminal::Pwsh,
            false,
            Some(std::path::PathBuf::from(
                r"C:\Program Files\PowerShell\7\pwsh.exe",
            )),
        );
        assert_eq!(program, r"C:\Program Files\PowerShell\7\pwsh.exe");
        assert!(args.is_empty());
    }

    #[test]
    fn pwsh_missing_falls_back_to_windows_powershell() {
        let (program, _) = windows_shell_for(&crate::models::DefaultTerminal::Pwsh, false, None);
        assert_eq!(program, "powershell.exe");
    }

    #[test]
    fn post_launch_settlement_keeps_unverified_listener_fail_closed() {
        let listener = crate::process::ports::ListenerIdentity::new(41_001, 41_001).unwrap();

        assert_eq!(
            classify_post_launch_listener_settlement(&[], |_| false),
            PostLaunchListenerSettlement::Pending
        );
        assert_eq!(
            classify_post_launch_listener_settlement(std::slice::from_ref(&listener), |_| false),
            PostLaunchListenerSettlement::Unverified
        );
    }

    #[test]
    fn post_launch_settlement_reports_proven_foreign_race_without_authorizing_kill() {
        let executable = std::env::current_exe().expect("test executable");
        let listener =
            crate::process::ports::ListenerIdentity::with_executable(41_002, 41_002, executable)
                .expect("listener identity");

        assert_eq!(
            classify_post_launch_listener_settlement(std::slice::from_ref(&listener), |_| false),
            PostLaunchListenerSettlement::Foreign
        );
        assert_eq!(
            classify_post_launch_listener_settlement(std::slice::from_ref(&listener), |_| true),
            PostLaunchListenerSettlement::Owned
        );
    }

    fn settlement_test_identity(
        pid: u32,
        creation_time_100ns: u64,
    ) -> (
        crate::process::ports::ListenerIdentity,
        crate::process::identity::ManagedProcessIdentity,
    ) {
        let executable = std::env::current_exe().expect("test executable");
        let listener = crate::process::ports::ListenerIdentity::with_executable(
            pid,
            creation_time_100ns,
            &executable,
        )
        .expect("listener identity");
        let identity = ManagedProcessIdentity::new(
            crate::process::identity::ManagedProcessId::new(pid, creation_time_100ns)
                .expect("test process id"),
            executable,
        )
        .expect("canonical test executable");
        (listener, identity)
    }

    #[test]
    fn post_launch_settlement_owns_listener_from_current_job_even_when_runtime_projection_lacks_pid(
    ) {
        let (listener, identity) = settlement_test_identity(41_010, 41_010);
        let job_members = [JobMemberObservation::Accessible { identity }];
        let stale_runtime_process_ids = vec![1_001_u32];

        assert!(
            !stale_runtime_process_ids.contains(&listener.pid()),
            "the 1s resource projection still lacks the Job member that bound the port"
        );
        assert!(listener_owned_by_current_job_member(
            &listener,
            &job_members
        ));
        assert_eq!(
            classify_post_launch_listener_settlement_against_job(
                std::slice::from_ref(&listener),
                &job_members,
            ),
            PostLaunchListenerSettlement::Owned
        );
        assert_eq!(
            classify_post_launch_settlement_with_job_authority(
                std::slice::from_ref(&listener),
                Ok(job_members.as_slice()),
                true,
            ),
            Ok(PostLaunchListenerSettlement::Owned)
        );
    }

    #[test]
    fn post_launch_settlement_job_authority_stays_fail_closed_for_foreign_unverified_and_query_loss(
    ) {
        let (owned_shape, _) = settlement_test_identity(41_011, 41_011);
        let (foreign, _) = settlement_test_identity(41_012, 41_012);
        let unverified = crate::process::ports::ListenerIdentity::new(41_013, 41_013)
            .expect("unproven listener");
        let inaccessible_same_pid = [JobMemberObservation::Inaccessible {
            pid: owned_shape.pid(),
            creation_time_100ns: Some(owned_shape.creation_time_100ns()),
            reason: "access_denied".to_string(),
        }];
        let wrong_creation = {
            let executable = std::env::current_exe().expect("test executable");
            [JobMemberObservation::Accessible {
                identity: ManagedProcessIdentity::new(
                    crate::process::identity::ManagedProcessId::new(owned_shape.pid(), 99_001)
                        .expect("mismatched creation"),
                    executable,
                )
                .expect("canonical test executable"),
            }]
        };
        let foreign_job = {
            let executable = std::env::current_exe().expect("test executable");
            [JobMemberObservation::Accessible {
                identity: ManagedProcessIdentity::new(
                    crate::process::identity::ManagedProcessId::new(7_001, 7_001)
                        .expect("foreign job member"),
                    executable,
                )
                .expect("canonical test executable"),
            }]
        };

        assert!(!listener_owned_by_current_job_member(
            &owned_shape,
            &inaccessible_same_pid
        ));
        assert_eq!(
            classify_post_launch_listener_settlement_against_job(
                std::slice::from_ref(&owned_shape),
                &inaccessible_same_pid,
            ),
            PostLaunchListenerSettlement::Unverified
        );
        assert_eq!(
            classify_post_launch_listener_settlement_against_job(
                std::slice::from_ref(&owned_shape),
                &wrong_creation,
            ),
            PostLaunchListenerSettlement::Unverified
        );
        assert_eq!(
            classify_post_launch_listener_settlement_against_job(
                std::slice::from_ref(&unverified),
                &foreign_job,
            ),
            PostLaunchListenerSettlement::Unverified
        );
        assert_eq!(
            classify_post_launch_listener_settlement_against_job(
                std::slice::from_ref(&foreign),
                &foreign_job,
            ),
            PostLaunchListenerSettlement::Foreign
        );
        assert_eq!(
            classify_post_launch_settlement_with_job_authority(
                std::slice::from_ref(&foreign),
                Ok(foreign_job.as_slice()),
                false,
            ),
            Err("settlement_generation_stale".to_string())
        );
        assert_eq!(
            classify_post_launch_settlement_with_job_authority(
                &[],
                Err("job_authority_unavailable"),
                true,
            ),
            Ok(PostLaunchListenerSettlement::Pending)
        );
        assert_eq!(
            classify_post_launch_settlement_with_job_authority(
                std::slice::from_ref(&foreign),
                Err("job_authority_unavailable"),
                true,
            ),
            Err("job_authority_unavailable".to_string())
        );
    }

    use crate::services::pid_file;
    use std::fs;
    use std::sync::Condvar;
    use std::thread;

    fn browser_test_launch(tool: SessionKind, command: &str) -> AiLaunchSpec {
        AiLaunchSpec {
            tab_id: "browser-ai-tab".to_string(),
            project_id: "browser-project".to_string(),
            tool,
            cwd: std::env::current_dir().unwrap(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: command.to_string(),
        }
    }

    fn ai_launch_test_project() -> Project {
        Project {
            id: "project-1".to_string(),
            name: "Project".to_string(),
            root_path: std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string(),
            folders: Vec::new(),
            color: None,
            pinned: Some(false),
            notes: None,
            save_log_files: Some(false),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn ai_launch_test_tab(
        tab_type: TabType,
        provider_session_id: Option<&str>,
        pty_session_id: Option<&str>,
    ) -> SessionTab {
        SessionTab {
            id: "ai-tab".to_string(),
            tab_type,
            project_id: "project-1".to_string(),
            command_id: None,
            pty_session_id: pty_session_id.map(str::to_string),
            provider_session_id: provider_session_id.map(str::to_string),
            label: Some("AI".to_string()),
            ssh_connection_id: None,
            browser_workspace: None,
        }
    }

    #[test]
    fn reconcile_saved_ai_tabs_preserves_runtime_provider_session_id() {
        let manager = ProcessManager::new();
        let mut runtime = SessionRuntimeState::new(
            "runtime-1",
            std::env::current_dir().unwrap(),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        runtime.configure_ai(AiLaunchSpec {
            tab_id: "tab-1".to_string(),
            project_id: "project-1".to_string(),
            tool: SessionKind::Claude,
            cwd: std::env::current_dir().unwrap(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        });
        runtime.status = SessionStatus::Running;
        runtime.provider_session_id = Some("provider-123".to_string());
        manager.register_runtime_session(runtime);
        let mut state = AppState::default();

        assert_eq!(manager.reconcile_saved_ai_tabs(&mut state), 1);
        assert_eq!(
            state
                .find_ai_tab("tab-1")
                .and_then(|tab| tab.provider_session_id.as_deref()),
            Some("provider-123")
        );
    }

    #[test]
    fn live_ai_process_session_for_tab_returns_only_exact_live_ai_runtime() {
        let manager = ProcessManager::new();
        let cwd = std::env::current_dir().unwrap();
        let mut live = SessionRuntimeState::new(
            "runtime-exact",
            cwd.clone(),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        live.configure_ai(AiLaunchSpec {
            tab_id: "task-exact".to_string(),
            project_id: "project-1".to_string(),
            tool: SessionKind::Codex,
            cwd: cwd.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "codex".to_string(),
        });
        live.status = SessionStatus::Running;
        manager.register_runtime_session(live);

        let mut stopped = SessionRuntimeState::new(
            "runtime-stopped",
            cwd,
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        stopped.configure_ai(AiLaunchSpec {
            tab_id: "task-stopped".to_string(),
            project_id: "project-1".to_string(),
            tool: SessionKind::Claude,
            cwd: std::env::current_dir().unwrap(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        });
        stopped.status = SessionStatus::Stopped;
        manager.register_runtime_session(stopped);

        assert_eq!(
            manager
                .live_ai_process_session_for_tab("task-exact")
                .as_deref(),
            Some("runtime-exact")
        );
        assert_eq!(
            manager.live_ai_process_session_for_tab("task-stopped"),
            None
        );
        assert_eq!(manager.live_ai_process_session_for_tab("missing"), None);
    }

    #[test]
    fn ai_launch_fresh_tabs_keep_configured_command_unchanged() {
        let project = ai_launch_test_project();
        let mut settings = Settings::default();
        settings.claude_command = Some("my-claude-wrapper --flag".to_string());
        settings.codex_command = Some("my-codex-wrapper --flag".to_string());

        let claude = build_ai_launch_spec(
            &settings,
            &project,
            &ai_launch_test_tab(TabType::Claude, None, Some("pty-1")),
            "pty-1",
        )
        .expect("fresh claude");
        let codex = build_ai_launch_spec(
            &settings,
            &project,
            &ai_launch_test_tab(TabType::Codex, None, Some("pty-1")),
            "pty-1",
        )
        .expect("fresh codex");

        assert_eq!(claude.startup_command, "my-claude-wrapper --flag");
        assert_eq!(codex.startup_command, "my-codex-wrapper --flag");
    }

    #[test]
    fn ai_launch_exact_resume_appends_provider_id_for_claude_and_codex() {
        let project = ai_launch_test_project();
        let mut settings = Settings::default();
        settings.claude_command = Some("claude".to_string());
        settings.codex_command = Some("codex --full-auto".to_string());

        let claude_exact = build_ai_launch_spec(
            &settings,
            &project,
            &ai_launch_test_tab(TabType::Claude, Some("provider-123"), None),
            "pty-1",
        )
        .expect("exact claude");
        let codex_exact = build_ai_launch_spec(
            &settings,
            &project,
            &ai_launch_test_tab(TabType::Codex, Some("provider-123"), None),
            "pty-1",
        )
        .expect("exact codex");

        assert!(claude_exact.startup_command.contains("--resume"));
        assert!(claude_exact.startup_command.contains("provider-123"));
        assert!(codex_exact.startup_command.contains("resume"));
        assert!(codex_exact.startup_command.contains("provider-123"));
        assert!(!codex_exact.startup_command.contains("--remote"));
    }

    #[test]
    fn ai_launch_legacy_restored_tabs_open_provider_resume_picker() {
        let project = ai_launch_test_project();
        let mut settings = Settings::default();
        settings.claude_command = Some("claude".to_string());
        settings.codex_command = Some("codex --full-auto".to_string());

        let claude_legacy = build_ai_launch_spec(
            &settings,
            &project,
            &ai_launch_test_tab(TabType::Claude, None, None),
            "pty-1",
        )
        .expect("legacy claude");
        let codex_legacy = build_ai_launch_spec(
            &settings,
            &project,
            &ai_launch_test_tab(TabType::Codex, None, None),
            "pty-1",
        )
        .expect("legacy codex");

        assert!(claude_legacy.startup_command.ends_with("--resume"));
        assert!(codex_legacy.startup_command.ends_with("resume"));
    }

    #[test]
    fn ai_launch_codex_exact_resume_composes_with_hook_injection() {
        let project = ai_launch_test_project();
        let mut settings = Settings::default();
        settings.codex_command = Some("codex --full-auto".to_string());
        let mut launch = build_ai_launch_spec(
            &settings,
            &project,
            &ai_launch_test_tab(TabType::Codex, Some("provider-123"), None),
            "pty-1",
        )
        .expect("exact codex launch");
        assert!(launch.startup_command.contains("resume"));
        assert!(launch.startup_command.contains("provider-123"));

        let manager = ProcessManager::new();
        manager.set_codex_hooks_support_probe_for_test(Arc::new(|_| Ok(())));
        manager.prepare_codex_launch_for_session(&mut launch, "codex-exact-resume");

        assert!(launch.startup_command.contains("resume"));
        assert!(launch.startup_command.contains("provider-123"));
        assert!(launch.startup_command.contains("codex-hook-relay"));
        assert!(launch
            .startup_command
            .contains("--dangerously-bypass-hook-trust"));
        assert!(!launch.startup_command.contains("--remote"));
        for event in crate::ai::codex_hooks::CODEX_HOOK_EVENTS {
            assert!(
                launch.startup_command.contains(&format!("hooks.{event}=")),
                "missing {event} in {}",
                launch.startup_command
            );
        }
        manager.cleanup_codex_adapter_session("codex-exact-resume");
    }

    #[test]
    fn ai_launch_rejects_malformed_provider_session_ids() {
        let project = ai_launch_test_project();
        let mut settings = Settings::default();
        settings.claude_command = Some("claude".to_string());

        for bad in ["bad\nid", "bad\rid", "bad;id", "%PATH%", &"x".repeat(257)] {
            let error = build_ai_launch_spec(
                &settings,
                &project,
                &ai_launch_test_tab(TabType::Claude, Some(bad), None),
                "pty-1",
            )
            .expect_err("malformed provider id");
            assert!(
                error.to_lowercase().contains("provider session id"),
                "unexpected error: {error}"
            );
        }
    }

    #[test]
    fn provider_session_id_binds_from_claude_and_codex_session_start_without_duplicate_revision() {
        let temp = temp_test_dir("provider-session-bind");
        let manager = ProcessManager::new();
        manager.set_codex_hooks_support_probe_for_test(Arc::new(|_| Ok(())));

        let mut claude_launch = AiLaunchSpec {
            tab_id: "claude-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut claude_launch, "claude-runtime", &temp);
        manager.ensure_runtime_entry("claude-runtime", temp.clone(), SessionDimensions::default());
        manager.update_session_state("claude-runtime", |state| {
            state.configure_ai(claude_launch.clone());
            state.status = SessionStatus::Running;
        });
        let claude_registration = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("claude-runtime")
            .map(|session| session.registration.clone())
            .expect("claude registration");
        let before_claude = manager.runtime_revision();
        let endpoint = manager.claude_hook_endpoint().unwrap();
        ureq::post(&endpoint)
            .header(
                "x-devmanager-claude-nonce",
                &claude_registration.nonce,
            )
            .send(
                br#"{"hook_event_name":"SessionStart","session_id":"claude-provider","source":"startup"}"#,
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && manager
                .runtime_state()
                .sessions
                .get("claude-runtime")
                .and_then(|session| session.provider_session_id.clone())
                .is_none()
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            manager
                .runtime_state()
                .sessions
                .get("claude-runtime")
                .and_then(|session| session.provider_session_id.clone())
                .as_deref(),
            Some("claude-provider")
        );
        assert!(
            !note_runtime_generation_change(&manager.inner, "claude-runtime"),
            "binding should consume the runtime generation change"
        );
        let after_claude = manager.runtime_revision();
        assert!(after_claude > before_claude);
        ureq::post(&endpoint)
            .header(
                "x-devmanager-claude-nonce",
                &claude_registration.nonce,
            )
            .send(
                br#"{"hook_event_name":"SessionStart","session_id":"claude-provider","source":"startup"}"#,
            )
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(manager.runtime_revision(), after_claude);

        let mut codex_launch = browser_test_launch(SessionKind::Codex, "codex --full-auto");
        manager.prepare_codex_launch_for_session(&mut codex_launch, "codex-runtime");
        manager.ensure_runtime_entry("codex-runtime", temp.clone(), SessionDimensions::default());
        manager.update_session_state("codex-runtime", |state| {
            state.configure_ai(codex_launch.clone());
            state.status = SessionStatus::Running;
        });
        let codex_registration = match manager
            .inner
            .codex_adapter_registry
            .lock()
            .unwrap()
            .sessions
            .get("codex-runtime")
            .expect("codex session")
        {
            CodexAdapterSession::Running { registration, .. } => registration.clone(),
            other => panic!("expected running codex session, got {other:?}"),
        };
        let before_codex = manager.runtime_revision();
        let body = serde_json::json!({
            "session_id": "codex-provider",
            "cwd": temp.to_string_lossy(),
            "transcript_path": null,
            "hook_event_name": "SessionStart"
        })
        .to_string();
        assert_eq!(
            manager.inner.codex_hook_registry.ingest(
                "127.0.0.1:9999".parse().unwrap(),
                &codex_registration.nonce,
                body.as_bytes(),
                1,
            ),
            crate::ai::codex_hooks::CodexRelayIngestStatus::Accepted
        );
        assert_eq!(
            manager
                .runtime_state()
                .sessions
                .get("codex-runtime")
                .and_then(|session| session.provider_session_id.clone())
                .as_deref(),
            Some("codex-provider")
        );
        assert!(
            !note_runtime_generation_change(&manager.inner, "codex-runtime"),
            "binding should consume the runtime generation change"
        );
        let after_codex = manager.runtime_revision();
        assert!(after_codex > before_codex);
        assert_eq!(
            manager.inner.codex_hook_registry.ingest(
                "127.0.0.1:9999".parse().unwrap(),
                &codex_registration.nonce,
                body.as_bytes(),
                2,
            ),
            crate::ai::codex_hooks::CodexRelayIngestStatus::Accepted
        );
        assert_eq!(manager.runtime_revision(), after_codex);

        let changed = serde_json::json!({
            "session_id": "codex-provider-2",
            "cwd": temp.to_string_lossy(),
            "transcript_path": null,
            "hook_event_name": "SessionStart"
        })
        .to_string();
        assert_eq!(
            manager.inner.codex_hook_registry.ingest(
                "127.0.0.1:9999".parse().unwrap(),
                &codex_registration.nonce,
                changed.as_bytes(),
                3,
            ),
            crate::ai::codex_hooks::CodexRelayIngestStatus::Rejected
        );
        assert_eq!(
            manager
                .runtime_state()
                .sessions
                .get("codex-runtime")
                .and_then(|session| session.provider_session_id.clone())
                .as_deref(),
            Some("codex-provider")
        );
        assert_eq!(manager.runtime_revision(), after_codex);
    }

    #[test]
    fn provider_session_binding_keeps_the_exact_runtime_resource() {
        let manager = ProcessManager::new();
        let task_id = TaskId::new();
        let agent_session_id = crate::domain::AgentSessionId::new();
        manager.ensure_runtime_entry(
            "provider-runtime",
            std::env::current_dir().unwrap(),
            SessionDimensions::default(),
        );
        manager.update_session_state("provider-runtime", |state| {
            state.status = SessionStatus::Running;
        });
        let fence = sealed_fence_issuer::issue(0x44, 9, ProcessOwner::Task(task_id), 42, 7);
        let resource_id = fence.resource().resource_id;
        manager
            .inner
            .provider_runtime
            .lock()
            .expect("provider runtime book")
            .live
            .insert(
                (resource_id, 9),
                ProviderLiveSession {
                    session_id: "provider-runtime".into(),
                    fence,
                    correlation: crate::providers::session::RuntimeCorrelation::sealed(
                        task_id,
                        agent_session_id,
                        ProviderKind::Codex,
                        9,
                        7,
                        crate::providers::session::LaunchNonce::new(),
                    ),
                    task_id,
                    agent_session_id,
                    provider_kind: ProviderKind::Codex,
                    provider_session_id: Some(
                        crate::domain::ProviderSessionId::new("codex-runtime-session")
                            .expect("provider session"),
                    ),
                    provider_identity_confirmed: false,
                    provider_identity_acceptance_started: false,
                    exit_reported: false,
                    settlement_kind: ProviderSettlementKind::ObserveExit,
                    settlement_failures: 0,
                    next_settlement_attempt: None,
                    failure_reported: false,
                },
            );

        assert!(manager.provider_session_bindings().is_empty());
        manager
            .inner
            .provider_runtime
            .lock()
            .expect("provider runtime book")
            .live
            .get_mut(&(resource_id, 9))
            .expect("live provider")
            .provider_identity_confirmed = true;
        assert_eq!(
            manager.provider_session_bindings(),
            vec![ProviderSessionBinding {
                task_id,
                agent_session_id,
                resource_id,
                provider_kind: ProviderKind::Codex,
                provider_session_id: crate::domain::ProviderSessionId::new("codex-runtime-session")
                    .expect("provider session"),
                runtime_generation: 9,
            }]
        );
    }

    #[test]
    fn duplicate_confirmed_provider_session_start_is_idempotent() {
        let manager = ProcessManager::new();
        let task_id = TaskId::new();
        let agent_session_id = AgentSessionId::new();
        let session_id = "duplicate-confirmed-session-start";
        let provider_session_id =
            crate::domain::ProviderSessionId::new("confirmed-provider-session")
                .expect("provider session");
        manager.ensure_runtime_entry(
            session_id,
            std::env::current_dir().unwrap(),
            SessionDimensions::default(),
        );
        manager.update_session_state(session_id, |state| {
            state.status = SessionStatus::Running;
            state.provider_session_id = Some(provider_session_id.as_str().to_owned());
        });
        let fence = sealed_fence_issuer::issue(0x47, 1, ProcessOwner::Task(task_id), 45, 10);
        let resource_id = fence.resource().resource_id;
        let correlation = crate::providers::session::RuntimeCorrelation::sealed(
            task_id,
            agent_session_id,
            ProviderKind::ClaudeCode,
            1,
            1,
            crate::providers::session::LaunchNonce::new(),
        );
        manager.inner.provider_runtime.lock().unwrap().live.insert(
            (resource_id, 1),
            ProviderLiveSession {
                session_id: session_id.to_string(),
                fence,
                correlation,
                task_id,
                agent_session_id,
                provider_kind: ProviderKind::ClaudeCode,
                provider_session_id: Some(provider_session_id.clone()),
                provider_identity_confirmed: true,
                provider_identity_acceptance_started: false,
                exit_reported: false,
                settlement_kind: ProviderSettlementKind::ObserveExit,
                settlement_failures: 0,
                next_settlement_attempt: None,
                failure_reported: false,
            },
        );
        let revision_before_duplicate = manager.runtime_revision();

        bind_runtime_provider_session_id(
            &manager.inner,
            session_id,
            provider_session_id.as_str().to_owned(),
        );

        let runtime = manager.runtime_state();
        let session = runtime
            .sessions
            .get(session_id)
            .expect("confirmed runtime remains visible");
        assert_eq!(session.status, SessionStatus::Running);
        assert_eq!(
            session.provider_session_id.as_deref(),
            Some(provider_session_id.as_str())
        );
        assert_eq!(manager.runtime_revision(), revision_before_duplicate);
        assert!(manager.drain_provider_session_failures().is_empty());
        let book = manager.inner.provider_runtime.lock().unwrap();
        let live = book.live.get(&(resource_id, 1)).expect("live provider");
        assert!(live.provider_identity_confirmed);
        assert!(!live.exit_reported);
    }

    #[test]
    fn early_provider_session_start_is_reconciled_after_live_publication() {
        let manager = ProcessManager::new();
        let task_id = TaskId::new();
        let agent_session_id = AgentSessionId::new();
        let session_id = "early-session-start";
        let provider_session_id = crate::domain::ProviderSessionId::new("early-provider-session")
            .expect("provider session");
        manager.ensure_runtime_entry(
            session_id,
            std::env::current_dir().unwrap(),
            SessionDimensions::default(),
        );
        manager.update_session_state(session_id, |state| {
            state.status = SessionStatus::Running;
        });

        bind_runtime_provider_session_id(
            &manager.inner,
            session_id,
            provider_session_id.as_str().to_owned(),
        );
        let early_runtime = manager.runtime_state();
        assert_eq!(
            early_runtime
                .sessions
                .get(session_id)
                .and_then(|session| session.provider_session_id.as_deref()),
            Some(provider_session_id.as_str())
        );
        assert!(manager.drain_provider_session_failures().is_empty());

        let fence = sealed_fence_issuer::issue(0x48, 1, ProcessOwner::Task(task_id), 46, 11);
        let resource_id = fence.resource().resource_id;
        let correlation = crate::providers::session::RuntimeCorrelation::sealed(
            task_id,
            agent_session_id,
            ProviderKind::ClaudeCode,
            1,
            1,
            crate::providers::session::LaunchNonce::new(),
        );
        manager.inner.provider_runtime.lock().unwrap().live.insert(
            (resource_id, 1),
            ProviderLiveSession {
                session_id: session_id.to_string(),
                fence,
                correlation,
                task_id,
                agent_session_id,
                provider_kind: ProviderKind::ClaudeCode,
                provider_session_id: None,
                provider_identity_confirmed: false,
                provider_identity_acceptance_started: false,
                exit_reported: false,
                settlement_kind: ProviderSettlementKind::ObserveExit,
                settlement_failures: 0,
                next_settlement_attempt: None,
                failure_reported: false,
            },
        );

        reconcile_provider_session_start_after_launch(&manager.inner, correlation);

        let runtime = manager.runtime_state();
        let session = runtime
            .sessions
            .get(session_id)
            .expect("failed exact runtime remains visible");
        assert_eq!(session.status, SessionStatus::Failed);
        assert_eq!(session.provider_session_id, None);
        assert_eq!(manager.drain_provider_session_failures().len(), 1);
        let book = manager.inner.provider_runtime.lock().unwrap();
        let live = book.live.get(&(resource_id, 1)).expect("live provider");
        assert!(live.provider_identity_acceptance_started);
        assert!(!live.provider_identity_confirmed);
    }

    #[test]
    fn provider_exit_settlement_retry_is_exponential_and_bounded() {
        assert_eq!(provider_exit_retry_delay(1), Duration::from_millis(250));
        assert_eq!(provider_exit_retry_delay(2), Duration::from_millis(500));
        assert_eq!(provider_exit_retry_delay(8), Duration::from_secs(30));
        assert_eq!(provider_exit_retry_delay(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn rejected_session_start_removes_adapter_reports_failure_and_leaves_exact_retry() {
        let manager = ProcessManager::new();
        let task_id = TaskId::new();
        let agent_session_id = AgentSessionId::new();
        let session_id = "rejected-session-start";
        let stable_session_key = StableSessionKey::from_tab(task_id.to_string());
        let registration = manager
            .inner
            .codex_hook_registry
            .register_expected(stable_session_key.clone(), None)
            .expect("relay registration");
        let identity = CodexAdapterIdentity {
            stable_session_key,
            generation: 1,
        };
        {
            let mut registry = manager.inner.codex_adapter_registry.lock().unwrap();
            registry.note_generation(&identity);
            registry.sessions.insert(
                session_id.to_string(),
                CodexAdapterSession::Running {
                    identity: identity.clone(),
                    registration: registration.clone(),
                    activated: false,
                    exact_resume: None,
                },
            );
        }
        manager.ensure_runtime_entry(
            session_id,
            std::env::current_dir().unwrap(),
            SessionDimensions::default(),
        );
        manager.update_session_state(session_id, |state| {
            state.status = SessionStatus::Running;
        });
        let fence = sealed_fence_issuer::issue(0x46, 1, ProcessOwner::Task(task_id), 44, 9);
        let resource_id = fence.resource().resource_id;
        manager.inner.provider_runtime.lock().unwrap().live.insert(
            (resource_id, 1),
            ProviderLiveSession {
                session_id: session_id.to_string(),
                fence,
                correlation: crate::providers::session::RuntimeCorrelation::sealed(
                    task_id,
                    agent_session_id,
                    ProviderKind::Codex,
                    1,
                    1,
                    crate::providers::session::LaunchNonce::new(),
                ),
                task_id,
                agent_session_id,
                provider_kind: ProviderKind::Codex,
                provider_session_id: None,
                provider_identity_confirmed: false,
                provider_identity_acceptance_started: false,
                exit_reported: false,
                settlement_kind: ProviderSettlementKind::ObserveExit,
                settlement_failures: 0,
                next_settlement_attempt: None,
                failure_reported: false,
            },
        );

        bind_runtime_provider_session_id(
            &manager.inner,
            session_id,
            "provider-conversation".to_string(),
        );

        assert!(!manager
            .inner
            .codex_adapter_registry
            .lock()
            .unwrap()
            .sessions
            .contains_key(session_id));
        assert_eq!(
            manager.inner.codex_hook_registry.ingest(
                "127.0.0.1:9999".parse().unwrap(),
                &registration.nonce,
                br#"{"session_id":"late","hook_event_name":"SessionStart"}"#,
                1,
            ),
            crate::ai::codex_hooks::CodexRelayIngestStatus::Rejected
        );
        let runtime = manager.runtime_state();
        let failed = runtime
            .sessions
            .get(session_id)
            .expect("failed runtime remains visible");
        assert_eq!(failed.status, SessionStatus::Failed);
        assert_eq!(failed.provider_session_id, None);
        assert!(failed
            .exit
            .as_ref()
            .is_some_and(|exit| exit.summary.contains("could not be persisted")));
        assert_eq!(
            manager.drain_provider_session_failures(),
            vec![ProviderSessionFailure {
                task_id,
                agent_session_id,
                provider_kind: ProviderKind::Codex,
                failure: crate::providers::session::ExactResumeFailure::ProviderRejected,
            }]
        );
        let book = manager.inner.provider_runtime.lock().unwrap();
        let live = book
            .live
            .get(&(resource_id, 1))
            .expect("exact lease remains retryable");
        assert_eq!(
            live.settlement_kind,
            ProviderSettlementKind::AbortRejectedSessionStart
        );
        assert_eq!(live.settlement_failures, 1);
        assert!(live.next_settlement_attempt.is_some());
        assert!(live.failure_reported);
        assert!(live.provider_identity_acceptance_started);
        assert!(!live.exit_reported);
    }

    #[test]
    fn codex_exact_resume_mismatch_fails_the_runtime_and_reports_the_task() {
        let manager = ProcessManager::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        })));
        let task_id = TaskId::new();
        let agent_session_id = crate::domain::AgentSessionId::new();
        let stable_session_key = StableSessionKey::from_tab(task_id.to_string());
        let expected = crate::domain::ProviderSessionId::new("expected-conversation").unwrap();
        let fence = sealed_fence_issuer::issue(0x45, 9, ProcessOwner::Task(task_id), 43, 8);
        let exact_resume = CodexExactResumeLaunchBinding {
            task_id,
            agent_session_id,
            resource_id: fence.resource().resource_id,
            runtime_generation: 9,
            provider_kind: ProviderKind::Codex,
            expected_provider_session_id: expected.clone(),
        };
        let registration = manager
            .inner
            .codex_hook_registry
            .register_expected(stable_session_key.clone(), Some(expected.clone()))
            .unwrap();
        let identity = CodexAdapterIdentity {
            stable_session_key,
            generation: 1,
        };
        {
            let mut registry = manager.inner.codex_adapter_registry.lock().unwrap();
            registry.note_generation(&identity);
            registry.sessions.insert(
                "codex-exact-mismatch".to_string(),
                CodexAdapterSession::Running {
                    identity: identity.clone(),
                    registration: registration.clone(),
                    activated: false,
                    exact_resume: Some(exact_resume),
                },
            );
        }
        manager.ensure_runtime_entry(
            "codex-exact-mismatch",
            std::env::current_dir().unwrap(),
            SessionDimensions::default(),
        );
        manager.update_session_state("codex-exact-mismatch", |state| {
            state.status = SessionStatus::Running;
            state.provider_session_id = Some(expected.as_str().to_string());
        });
        manager.inner.provider_runtime.lock().unwrap().live.insert(
            (fence.resource().resource_id, 9),
            ProviderLiveSession {
                session_id: "codex-exact-mismatch".into(),
                fence,
                correlation: crate::providers::session::RuntimeCorrelation::sealed(
                    task_id,
                    agent_session_id,
                    ProviderKind::Codex,
                    9,
                    8,
                    crate::providers::session::LaunchNonce::new(),
                ),
                task_id,
                agent_session_id,
                provider_kind: ProviderKind::Codex,
                provider_session_id: Some(expected),
                provider_identity_confirmed: false,
                provider_identity_acceptance_started: false,
                exit_reported: false,
                settlement_kind: ProviderSettlementKind::ObserveExit,
                settlement_failures: 0,
                next_settlement_attempt: None,
                failure_reported: false,
            },
        );

        assert_eq!(
            manager.inner.codex_hook_registry.ingest(
                "127.0.0.1:9999".parse().unwrap(),
                &registration.nonce,
                &serde_json::to_vec(&serde_json::json!({
                    "session_id": "different-conversation",
                    "hook_event_name": "SessionStart",
                    "cwd": "C:\\proj",
                    "transcript_path": null
                }))
                .unwrap(),
                1,
            ),
            crate::ai::codex_hooks::CodexRelayIngestStatus::Rejected
        );
        let runtime = manager.runtime_state();
        let failed = runtime
            .sessions
            .get("codex-exact-mismatch")
            .expect("failed runtime remains visible");
        assert_eq!(failed.status, SessionStatus::Failed);
        assert!(failed
            .exit
            .as_ref()
            .is_some_and(|exit| exit.summary.contains("Exact provider resume failed")));
        assert_eq!(
            manager.drain_provider_session_failures(),
            vec![ProviderSessionFailure {
                task_id,
                agent_session_id,
                provider_kind: ProviderKind::Codex,
                failure: crate::providers::session::ExactResumeFailure::ProviderRejected,
            }]
        );
        assert!(!manager
            .inner
            .codex_adapter_registry
            .lock()
            .unwrap()
            .sessions
            .contains_key("codex-exact-mismatch"));
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::CodexAdapterRemoved { identity: removed }
                if removed == &codex_semantic_identity("codex-exact-mismatch", &identity)
        )));
        assert_eq!(
            manager.inner.codex_hook_registry.ingest(
                "127.0.0.1:9999".parse().unwrap(),
                &registration.nonce,
                br#"{"session_id":"different-conversation","hook_event_name":"Stop","cwd":"C:\\proj"}"#,
                2,
            ),
            crate::ai::codex_hooks::CodexRelayIngestStatus::Rejected
        );
    }

    #[test]
    fn codex_exact_resume_mismatch_before_live_publication_is_latched_and_removes_adapter() {
        let manager = ProcessManager::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        })));
        let task_id = TaskId::new();
        let agent_session_id = AgentSessionId::new();
        let expected = crate::domain::ProviderSessionId::new("expected-pre-live").unwrap();
        let fence = sealed_fence_issuer::issue(0x46, 11, ProcessOwner::Task(task_id), 44, 9);
        let exact_resume = CodexExactResumeLaunchBinding {
            task_id,
            agent_session_id,
            resource_id: fence.resource().resource_id,
            runtime_generation: 11,
            provider_kind: ProviderKind::Codex,
            expected_provider_session_id: expected.clone(),
        };
        let stable_session_key = StableSessionKey::from_tab(task_id.to_string());
        let registration = manager
            .inner
            .codex_hook_registry
            .register_expected(stable_session_key.clone(), Some(expected))
            .unwrap();
        let identity = CodexAdapterIdentity {
            stable_session_key,
            generation: 1,
        };
        {
            let mut registry = manager.inner.codex_adapter_registry.lock().unwrap();
            registry.note_generation(&identity);
            registry.sessions.insert(
                "codex-pre-live-mismatch".to_string(),
                CodexAdapterSession::Running {
                    identity: identity.clone(),
                    registration: registration.clone(),
                    activated: false,
                    exact_resume: Some(exact_resume.clone()),
                },
            );
        }
        manager.ensure_runtime_entry(
            "codex-pre-live-mismatch",
            std::env::current_dir().unwrap(),
            SessionDimensions::default(),
        );
        manager.update_session_state("codex-pre-live-mismatch", |state| {
            state.status = SessionStatus::Starting;
        });

        assert_eq!(
            manager.inner.codex_hook_registry.ingest(
                "127.0.0.1:9999".parse().unwrap(),
                &registration.nonce,
                &serde_json::to_vec(&serde_json::json!({
                    "session_id": "different-conversation",
                    "hook_event_name": "SessionStart",
                    "cwd": "C:\\proj",
                    "transcript_path": null
                }))
                .unwrap(),
                1,
            ),
            crate::ai::codex_hooks::CodexRelayIngestStatus::Rejected
        );

        assert!(!manager
            .inner
            .codex_adapter_registry
            .lock()
            .unwrap()
            .sessions
            .contains_key("codex-pre-live-mismatch"));
        assert_eq!(
            take_latched_codex_exact_resume_failure(&manager.inner, exact_resume.key()),
            Some(exact_resume.clone())
        );
        queue_provider_session_failure(&manager.inner, exact_resume.task_failure());
        assert_eq!(
            manager.drain_provider_session_failures(),
            vec![exact_resume.task_failure()]
        );
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::CodexAdapterRemoved { identity: removed }
                if removed == &codex_semantic_identity("codex-pre-live-mismatch", &identity)
        )));
    }

    #[test]
    fn codex_exact_resume_failure_retires_old_nonce_without_removing_replacement() {
        let manager = ProcessManager::new();
        let task_id = TaskId::new();
        let agent_session_id = AgentSessionId::new();
        let stable_session_key = StableSessionKey::from_tab(task_id.to_string());
        let old_expected = crate::domain::ProviderSessionId::new("old-conversation").unwrap();
        let old_registration = manager
            .inner
            .codex_hook_registry
            .register_expected(stable_session_key.clone(), Some(old_expected.clone()))
            .unwrap();
        let old_identity = CodexAdapterIdentity {
            stable_session_key: stable_session_key.clone(),
            generation: 1,
        };
        let old_fence = sealed_fence_issuer::issue(0x47, 12, ProcessOwner::Task(task_id), 45, 10);
        let old_exact_resume = CodexExactResumeLaunchBinding {
            task_id,
            agent_session_id,
            resource_id: old_fence.resource().resource_id,
            runtime_generation: 12,
            provider_kind: ProviderKind::Codex,
            expected_provider_session_id: old_expected,
        };
        {
            let mut registry = manager.inner.codex_adapter_registry.lock().unwrap();
            registry.note_generation(&old_identity);
            registry.sessions.insert(
                "codex-replaced-mismatch".to_string(),
                CodexAdapterSession::Running {
                    identity: old_identity.clone(),
                    registration: old_registration.clone(),
                    activated: false,
                    exact_resume: Some(old_exact_resume.clone()),
                },
            );
        }

        // The mismatch handler has already located the old identity. A newer
        // launch now replaces that adapter before the handler can clean it up.
        let replacement_expected =
            crate::domain::ProviderSessionId::new("replacement-conversation").unwrap();
        let replacement_registration = manager
            .inner
            .codex_hook_registry
            .register_expected(
                stable_session_key.clone(),
                Some(replacement_expected.clone()),
            )
            .unwrap();
        let replacement_identity = CodexAdapterIdentity {
            stable_session_key,
            generation: 2,
        };
        let replacement_fence =
            sealed_fence_issuer::issue(0x48, 13, ProcessOwner::Task(task_id), 46, 11);
        let replacement_exact_resume = CodexExactResumeLaunchBinding {
            task_id,
            agent_session_id,
            resource_id: replacement_fence.resource().resource_id,
            runtime_generation: 13,
            provider_kind: ProviderKind::Codex,
            expected_provider_session_id: replacement_expected,
        };
        {
            let mut registry = manager.inner.codex_adapter_registry.lock().unwrap();
            registry.note_generation(&replacement_identity);
            registry.sessions.insert(
                "codex-replaced-mismatch".to_string(),
                CodexAdapterSession::Running {
                    identity: replacement_identity.clone(),
                    registration: replacement_registration.clone(),
                    activated: false,
                    exact_resume: Some(replacement_exact_resume),
                },
            );
        }

        handle_codex_exact_resume_failed(
            &manager.inner,
            "codex-replaced-mismatch",
            &old_identity,
            &old_registration,
            Some(&old_exact_resume),
        );

        assert!(manager
            .inner
            .codex_adapter_registry
            .lock()
            .unwrap()
            .sessions
            .get("codex-replaced-mismatch")
            .is_some_and(|session| session.identity() == &replacement_identity));
        assert!(manager
            .inner
            .codex_hook_registry
            .unregister(&old_registration.nonce)
            .is_none());
        assert!(manager
            .inner
            .codex_hook_registry
            .unregister(&replacement_registration.nonce)
            .is_some());
    }

    fn browser_provider_replay_plan(
        label: &str,
        with_secret: bool,
    ) -> crate::browser::BrowserReplayPlan {
        use crate::browser::{
            compile_browser_replay, BrowserRecipeAction, BrowserRecipeInput,
            BrowserRecipeInputKind, BrowserRecipeLocator, BrowserRecipeStep, BrowserRecipeV1,
            BrowserRecipeValue, BrowserRecipeViewport, BROWSER_RECIPE_SCHEMA_VERSION,
        };

        let inputs = with_secret
            .then(|| BrowserRecipeInput {
                name: "password".to_string(),
                kind: BrowserRecipeInputKind::Secret,
                default_value: None,
            })
            .into_iter()
            .collect();
        let action = if with_secret {
            BrowserRecipeAction::Type {
                locator: BrowserRecipeLocator {
                    test_id: Some("password".to_string()),
                    ..BrowserRecipeLocator::default()
                },
                value: BrowserRecipeValue::Input {
                    name: "password".to_string(),
                },
            }
        } else {
            BrowserRecipeAction::Reload
        };
        compile_browser_replay(
            &BrowserRecipeV1 {
                schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
                id: format!("provider-lifecycle-{label}"),
                name: "Provider lifecycle".to_string(),
                description: "Exact process-exit lease fixture".to_string(),
                start_url: "https://example.test/provider".to_string(),
                viewport: BrowserRecipeViewport::default(),
                inputs,
                steps: vec![BrowserRecipeStep {
                    id: "provider-step".to_string(),
                    action,
                    wait: None,
                    assertions: Vec::new(),
                }],
            },
            Vec::new(),
        )
        .unwrap()
    }

    fn browser_attachment_snapshot(annotation_id: &str) -> BrowserWorkspaceSnapshot {
        serde_json::from_value(serde_json::json!({
            "annotations": [{
                "id": annotation_id,
                "kind": "element",
                "tabId": "page",
                "anchorRevision": 1,
                "comment": format!("Review {annotation_id}"),
                "url": "https://example.test/page?token=secret",
                "locator": {},
                "bounds": { "x": 1, "y": 2, "width": 30, "height": 40 },
                "viewport": {},
                "screenshotResource": format!("shot-{annotation_id}"),
                "computedStyles": {},
                "resolved": false
            }],
            "pendingAnnotationRevision": 1,
            "pendingAnnotationIds": [annotation_id]
        }))
        .expect("valid attachment snapshot")
    }

    fn stop_background_tasks_for_test(manager: &ProcessManager) {
        manager.inner.background_stop.store(true, Ordering::SeqCst);
        if let Some(handle) = manager
            .inner
            .background_thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            handle.join().expect("background task stops cleanly");
        }
    }

    fn lifecycle_state_for_test(lifecycle: &ProcessManagerHandleLifecycle) -> (usize, bool) {
        let state = lifecycle
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (state.active_handles, state.shutting_down)
    }

    fn configure_auto_restart_race(manager: &ProcessManager, command_id: &str) -> ServerLaunchSpec {
        stop_background_tasks_for_test(manager);
        manager.inner.background_stop.store(false, Ordering::SeqCst);

        let launch = ServerLaunchSpec {
            command_id: command_id.to_string(),
            project_id: "project".to_string(),
            port: None,
            cwd: std::env::current_dir().unwrap(),
            program: "definitely-not-a-devmanager-server".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            auto_restart: true,
            log_file_path: None,
        };
        let mut session = SessionRuntimeState::new(
            launch.command_id.clone(),
            launch.cwd.clone(),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.status = SessionStatus::Crashed;
        session.configure_server(launch.clone());
        manager.register_runtime_session(session);
        manager
            .inner
            .restart_backoffs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                launch.command_id.clone(),
                RestartBackoff {
                    delay: Duration::ZERO,
                    last_crash: Instant::now(),
                },
            );
        launch
    }

    #[test]
    fn empty_ai_restart_command_fails_preflight_without_mutating_the_tab_or_runtime() {
        let manager = ProcessManager::new();
        let mut state = AppState::default();
        state.config.settings.claude_command = Some("   ".to_string());
        state.config.projects.push(Project {
            id: "restart-project".to_string(),
            name: "Restart project".to_string(),
            root_path: std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string(),
            folders: Vec::new(),
            color: None,
            pinned: Some(false),
            notes: None,
            save_log_files: Some(false),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        });
        state.open_tabs.push(SessionTab {
            id: "restart-tab".to_string(),
            tab_type: TabType::Claude,
            project_id: "restart-project".to_string(),
            command_id: None,
            pty_session_id: Some("existing-session".to_string()),
            provider_session_id: None,
            label: Some("Claude".to_string()),
            ssh_connection_id: None,
            browser_workspace: None,
        });

        assert_eq!(
            manager.validate_ai_restart(&state, "restart-tab"),
            Err("AI command is empty".to_string())
        );
        assert_eq!(
            state
                .find_ai_tab("restart-tab")
                .and_then(|tab| tab.pty_session_id.as_deref()),
            Some("existing-session")
        );
        assert!(manager.runtime_state().sessions.is_empty());
    }

    #[test]
    fn blank_server_launches_leave_tabs_runtime_and_process_queue_untouched() {
        let cwd = temp_test_dir("blank-server-launch-preflight");
        for operation in ["start", "restart"] {
            let manager = ProcessManager::new();
            let mut state = app_state_with_server(&cwd, true);
            state.config.projects[0].folders[0].commands[0].command = " \t ".to_string();
            let tabs_before = state.open_tabs.clone();
            let active_before = state.active_tab_id.clone();
            let runtime_before = manager.runtime_state();
            let revision_before = manager.runtime_revision();

            let result = match operation {
                "start" => {
                    manager.start_server(&mut state, "server-cmd", SessionDimensions::default())
                }
                "restart" => {
                    manager.restart_server(&mut state, "server-cmd", SessionDimensions::default())
                }
                _ => unreachable!(),
            };

            assert_eq!(
                result,
                Err("Server command `server-cmd` is empty".to_string()),
                "{operation}"
            );
            assert_eq!(state.open_tabs, tabs_before, "{operation}");
            assert_eq!(state.active_tab_id, active_before, "{operation}");
            let runtime_after = manager.runtime_state();
            assert_eq!(
                runtime_after.sessions.len(),
                runtime_before.sessions.len(),
                "{operation}"
            );
            assert_eq!(
                runtime_after.active_session_id, runtime_before.active_session_id,
                "{operation}"
            );
            assert_eq!(manager.runtime_revision(), revision_before, "{operation}");
            assert!(
                manager.drain_process_op_completions().is_empty(),
                "{operation}"
            );
            stop_background_tasks_for_test(&manager);
        }
    }

    #[test]
    fn oversized_restart_banner_is_rejected_before_state_or_queue_mutation() {
        let cwd = temp_test_dir("oversized-restart-banner");
        let manager = ProcessManager::new();
        let mut state = app_state_with_server(&cwd, true);
        let tabs_before = state.open_tabs.clone();
        let runtime_before = manager.runtime_state();
        let revision_before = manager.runtime_revision();
        let banner = "x".repeat(MAX_PROCESS_OP_HOST_STRING_BYTES + 1);

        let error = manager
            .restart_server_with_banner(
                &mut state,
                "server-cmd",
                SessionDimensions::default(),
                &banner,
            )
            .expect_err("oversized host strings must fail before operation admission");
        assert!(error.contains("restart banner"), "{error}");
        assert_eq!(state.open_tabs, tabs_before);
        let runtime_after = manager.runtime_state();
        assert_eq!(runtime_after.sessions.len(), runtime_before.sessions.len());
        assert_eq!(
            runtime_after.active_session_id,
            runtime_before.active_session_id
        );
        assert_eq!(manager.runtime_revision(), revision_before);
        assert!(manager.drain_process_op_completions().is_empty());
    }

    #[test]
    fn attachment_binding_precedes_gateway_and_survives_provider_setup_failure() {
        let manager = ProcessManager::new();
        manager.set_codex_hooks_support_probe_for_test(Arc::new(|_| {
            Err("fixture probe failed".to_string())
        }));
        let mut launch = browser_test_launch(SessionKind::Codex, "codex --full-auto");
        let binding = manager
            .prepare_browser_launch_for_session(
                &mut launch,
                "attachment-no-gateway",
                browser_attachment_snapshot("ann-no-gateway"),
            )
            .expect("AI launch binds attachments without a gateway");

        assert_eq!(
            manager
                .browser_attachment_broker()
                .binding("attachment-no-gateway"),
            Some(binding.clone())
        );
        assert!(manager
            .browser_attachment_broker()
            .reserve_for_input(
                "attachment-no-gateway",
                crate::browser::BrowserPromptInput::Text("prompt")
            )
            .is_some());
        let _ = manager.prepare_codex_launch_for_session(&mut launch, "attachment-no-gateway");
        assert_eq!(
            manager
                .browser_attachment_broker()
                .binding("attachment-no-gateway"),
            Some(binding)
        );
    }

    #[test]
    fn local_ai_tab_close_fully_retires_only_its_attachment_workspace() {
        let manager = ProcessManager::new();
        let mut state = AppState::default();
        for (tab_id, annotation_id) in [("tab-a", "ann-a"), ("tab-b", "ann-b")] {
            state.open_tabs.push(SessionTab {
                id: tab_id.to_string(),
                tab_type: TabType::Claude,
                project_id: "project".to_string(),
                command_id: None,
                pty_session_id: None,
                provider_session_id: None,
                label: None,
                ssh_connection_id: None,
                browser_workspace: Some(browser_attachment_snapshot(annotation_id)),
            });
        }
        let key_a = BrowserWorkspaceKey::new("project", "tab-a").unwrap();
        let key_b = BrowserWorkspaceKey::new("project", "tab-b").unwrap();
        let broker = manager.browser_attachment_broker();
        broker.observe_workspace(key_a.clone(), state.browser_workspace("tab-a").unwrap());
        broker.observe_workspace(key_b.clone(), state.browser_workspace("tab-b").unwrap());
        broker.bind_session("binding-a", key_a.clone());
        broker.bind_session("binding-b", key_b.clone());

        manager.close_ai_session(&mut state, "tab-a").unwrap();

        assert!(state.find_tab("tab-a").is_none());
        assert!(state.find_tab("tab-b").is_some());
        assert!(broker.binding("binding-a").is_none());
        assert!(broker.projection(&key_a).pending_annotation_ids.is_empty());
        assert!(broker.binding("binding-b").is_some());
        assert_eq!(
            broker.projection(&key_b).pending_annotation_ids,
            vec!["ann-b"]
        );
    }

    #[test]
    fn replacement_and_same_id_fallback_fence_stale_attachment_cleanup() {
        let manager = ProcessManager::new();
        let mut old_launch = browser_test_launch(SessionKind::Claude, "claude --model sonnet");
        let old = manager
            .prepare_browser_launch_for_session(
                &mut old_launch,
                "attachment-old",
                browser_attachment_snapshot("ann-restart"),
            )
            .unwrap();
        manager.ensure_runtime_entry(
            "attachment-old",
            std::env::current_dir().unwrap(),
            SessionDimensions::default(),
        );
        manager.update_session_state("attachment-old", |state| {
            state.status = SessionStatus::Running;
        });
        let old_exit = session_change_notifier(manager.inner.clone(), "attachment-old".into());

        let mut replacement_launch =
            browser_test_launch(SessionKind::Claude, "claude --model opus");
        let replacement = manager
            .prepare_browser_launch_for_session(
                &mut replacement_launch,
                "attachment-replacement",
                BrowserWorkspaceSnapshot::default(),
            )
            .unwrap();
        manager.update_session_state("attachment-old", |state| {
            state.status = SessionStatus::Exited;
        });
        old_exit();
        assert_eq!(
            manager
                .browser_attachment_broker()
                .binding("attachment-replacement"),
            Some(replacement)
        );
        assert!(!manager.browser_attachment_broker().unbind_if_matches(&old));
    }

    #[test]
    fn queue_failure_unbinds_only_the_captured_attachment_generation() {
        let manager = ProcessManager::new();
        manager.op_queue.shutdown();
        let mut launch = browser_test_launch(SessionKind::Claude, "claude --model sonnet");
        let binding = manager
            .prepare_browser_launch_for_session(
                &mut launch,
                "attachment-queue-failure",
                browser_attachment_snapshot("ann-queue"),
            )
            .unwrap();

        let result = manager.schedule_spawn_ai(
            &launch,
            "attachment-queue-failure",
            SessionDimensions::default(),
            false,
            None,
            binding,
        );

        assert!(result.is_err());
        assert!(manager
            .browser_attachment_broker()
            .binding("attachment-queue-failure")
            .is_none());
        stop_background_tasks_for_test(&manager);
    }

    #[test]
    fn close_queue_failure_unbinds_only_the_captured_attachment_generation() {
        let manager = ProcessManager::new();
        manager.op_queue.shutdown();
        let mut launch = browser_test_launch(SessionKind::Claude, "claude --model sonnet");
        let binding = manager
            .prepare_browser_launch_for_session(
                &mut launch,
                "attachment-close-queue-failure",
                browser_attachment_snapshot("ann-close-queue"),
            )
            .unwrap();

        let result = manager.schedule_close_ai("attachment-close-queue-failure", None);

        assert!(result.is_err());
        assert!(!manager
            .browser_attachment_broker()
            .unbind_if_matches(&binding));
        assert!(manager
            .browser_attachment_broker()
            .binding("attachment-close-queue-failure")
            .is_none());
        stop_background_tasks_for_test(&manager);
    }

    #[test]
    fn user_origin_inputs_share_one_attachment_transaction_and_retry_failures() {
        let manager = ProcessManager::new();
        let broker = manager.browser_attachment_broker();
        let key = BrowserWorkspaceKey::new("project", "conversation").unwrap();
        broker.observe_workspace(key.clone(), &browser_attachment_snapshot("ann-transaction"));
        broker.bind_session("transaction-session", key.clone());

        let mut control_payload = None;
        coordinate_user_origin_write(
            &broker,
            "transaction-session",
            crate::browser::BrowserPromptInput::RawBytes(b"\x03"),
            |prefix| {
                control_payload = Some(prefix.to_string());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(control_payload.as_deref(), Some(""));
        assert_eq!(
            broker.projection(&key).pending_annotation_ids,
            ["ann-transaction"]
        );

        let error = coordinate_user_origin_write(
            &broker,
            "transaction-session",
            crate::browser::BrowserPromptInput::Paste("first try"),
            |prefix| {
                assert!(prefix.contains("ann-transaction"));
                Err("fixture write or flush failed".to_string())
            },
        )
        .expect_err("failed compound write rolls back");
        assert!(error.contains("fixture write or flush failed"));

        let mut successful_prefix = String::new();
        coordinate_user_origin_write(
            &broker,
            "transaction-session",
            crate::browser::BrowserPromptInput::Text("retry"),
            |prefix| {
                successful_prefix = prefix.to_string();
                Ok(())
            },
        )
        .unwrap();
        assert!(successful_prefix.contains("ann-transaction"));
        assert!(broker.projection(&key).pending_annotation_ids.is_empty());

        let mut later_enter_prefix = None;
        coordinate_user_origin_write(
            &broker,
            "transaction-session",
            crate::browser::BrowserPromptInput::RawBytes(b"\r"),
            |prefix| {
                later_enter_prefix = Some(prefix.to_string());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(later_enter_prefix.as_deref(), Some(""));
    }

    #[test]
    fn user_origin_transactions_are_isolated_by_session_and_workspace() {
        let broker = crate::browser::BrowserAttachmentBroker::default();
        let first_key = BrowserWorkspaceKey::new("project", "first").unwrap();
        let second_key = BrowserWorkspaceKey::new("project", "second").unwrap();
        broker.observe_workspace(first_key.clone(), &browser_attachment_snapshot("ann-first"));
        broker.observe_workspace(
            second_key.clone(),
            &browser_attachment_snapshot("ann-second"),
        );
        broker.bind_session("first-session", first_key.clone());
        broker.bind_session("second-session", second_key.clone());

        coordinate_user_origin_write(
            &broker,
            "first-session",
            crate::browser::BrowserPromptInput::RawBytes("hello".as_bytes()),
            |prefix| {
                assert!(prefix.contains("ann-first"));
                assert!(!prefix.contains("ann-second"));
                Ok(())
            },
        )
        .unwrap();

        assert!(broker
            .projection(&first_key)
            .pending_annotation_ids
            .is_empty());
        assert_eq!(
            broker.projection(&second_key).pending_annotation_ids,
            ["ann-second"]
        );
        coordinate_user_origin_write(
            &broker,
            "second-session",
            crate::browser::BrowserPromptInput::Paste("world"),
            |prefix| {
                assert!(prefix.contains("ann-second"));
                assert!(!prefix.contains("ann-first"));
                Ok(())
            },
        )
        .unwrap();
        assert!(broker
            .projection(&second_key)
            .pending_annotation_ids
            .is_empty());
    }

    #[test]
    fn browser_provider_registration_injects_claude_ephemerally_and_cleans_up() {
        let (bridge, _inbox) = crate::browser::browser_command_channel(8);
        let gateway = crate::browser::BrowserGatewayHandle::start(bridge).unwrap();
        let manager = ProcessManager::new();
        manager.set_browser_gateway_registrar(Some(gateway.registrar()));
        let mut launch = browser_test_launch(
            SessionKind::Claude,
            "claude --model sonnet --dangerously-skip-permissions",
        );
        let original = launch.startup_command.clone();

        manager.prepare_browser_launch_for_session(
            &mut launch,
            "claude-browser-session",
            crate::browser::BrowserWorkspaceSnapshot::default(),
        );
        manager.prepare_claude_launch_for_session(
            &mut launch,
            "claude-browser-session",
            &manager.inner.claude_hook_temp_root,
        );

        assert!(launch.startup_command.starts_with(&original));
        assert!(launch.startup_command.contains("--mcp-config"));
        assert!(launch.startup_command.contains("--settings"));
        let sessions = manager.inner.browser_provider_sessions.lock().unwrap();
        let provider = sessions.get("claude-browser-session").unwrap();
        let token = provider
            .registration
            .access()
            .bearer_token_for_launch()
            .to_string();
        let overlay_path = provider
            ._claude_overlay
            .as_ref()
            .unwrap()
            .path()
            .to_path_buf();
        let overlay = std::fs::read_to_string(&overlay_path).unwrap();
        assert!(overlay.contains("${DEVMANAGER_BROWSER_TOKEN}"));
        assert!(!overlay.contains(&token));
        drop(sessions);
        assert_eq!(
            manager
                .browser_environment("claude-browser-session")
                .get(crate::browser::DEVMANAGER_BROWSER_TOKEN_ENV),
            Some(&token)
        );
        assert!(!serde_json::to_string(&manager.runtime_state())
            .unwrap()
            .contains(&token));

        manager.cleanup_ai_adapters_for_session("claude-browser-session");
        assert!(!overlay_path.exists());
        assert_eq!(gateway.registrar().active_registration_count(), 0);
    }

    #[test]
    fn browser_provider_failure_keeps_launch_and_environment_exact() {
        let (bridge, _inbox) = crate::browser::browser_command_channel(8);
        let gateway = crate::browser::BrowserGatewayHandle::start(bridge).unwrap();
        let manager = ProcessManager::new();
        manager.set_browser_gateway_registrar(Some(gateway.registrar()));
        let mut launch = browser_test_launch(SessionKind::Claude, "claude | Write-Output nope");
        let original = launch.clone();

        manager.prepare_browser_launch_for_session(
            &mut launch,
            "claude-browser-failure",
            crate::browser::BrowserWorkspaceSnapshot::default(),
        );

        assert_eq!(launch.startup_command, original.startup_command);
        assert!(manager
            .browser_environment("claude-browser-failure")
            .is_empty());
        assert_eq!(gateway.registrar().active_registration_count(), 0);
        let diagnostic = manager
            .browser_diagnostic(&launch.tab_id)
            .expect("matching browser diagnostic");
        assert!(diagnostic
            .to_ascii_lowercase()
            .contains("browser tools unavailable"));
        assert!(!diagnostic.contains("Bearer"));
    }

    #[test]
    fn explicit_browser_provider_drain_revokes_all_sessions_and_owned_overlays() {
        let (bridge, _inbox) = crate::browser::browser_command_channel(8);
        let gateway = crate::browser::BrowserGatewayHandle::start(bridge).unwrap();
        let manager = ProcessManager::new();
        manager.set_browser_gateway_registrar(Some(gateway.registrar()));
        let mut launch = browser_test_launch(SessionKind::Claude, "claude --model sonnet");
        manager.prepare_browser_launch_for_session(
            &mut launch,
            "claude-browser-drain",
            crate::browser::BrowserWorkspaceSnapshot::default(),
        );
        let overlay_path = manager
            .inner
            .browser_provider_sessions
            .lock()
            .unwrap()
            .get("claude-browser-drain")
            .unwrap()
            ._claude_overlay
            .as_ref()
            .unwrap()
            .path()
            .to_path_buf();
        assert!(overlay_path.exists());
        assert_eq!(gateway.registrar().active_registration_count(), 1);

        manager.drain_browser_provider_adapter();

        assert!(!overlay_path.exists());
        assert!(manager
            .inner
            .browser_provider_sessions
            .lock()
            .unwrap()
            .is_empty());
        assert_eq!(gateway.registrar().active_registration_count(), 0);
    }

    #[tokio::test]
    async fn terminal_exit_cleans_only_the_captured_browser_provider_registration() {
        use crate::browser::{
            BrowserCommand, BrowserError, BrowserReplaySecretError, BrowserReplaySecretPromptVault,
            BrowserReplayStatus, BrowserResponse, BrowserWorkspaceKey,
        };

        let (bridge, mut inbox) = crate::browser::browser_command_channel(8);
        let gateway = crate::browser::BrowserGatewayHandle::start(bridge.clone()).unwrap();
        let manager = ProcessManager::new();
        manager.set_browser_gateway_registrar(Some(gateway.registrar()));
        let session_id = "shared-browser-exit-session";
        let mut launch = browser_test_launch(SessionKind::Claude, "claude --model sonnet");
        manager.prepare_browser_launch_for_session(
            &mut launch,
            session_id,
            crate::browser::BrowserWorkspaceSnapshot::default(),
        );
        manager.ensure_runtime_entry(
            session_id,
            std::env::current_dir().unwrap(),
            SessionDimensions::default(),
        );
        manager.update_session_state(session_id, |state| {
            state.status = SessionStatus::Running;
        });
        let old_exit_notifier =
            session_change_notifier(manager.inner.clone(), session_id.to_string());
        let old_overlay = manager
            .inner
            .browser_provider_sessions
            .lock()
            .unwrap()
            .get(session_id)
            .unwrap()
            ._claude_overlay
            .as_ref()
            .unwrap()
            .path()
            .to_path_buf();

        let mut replacement = browser_test_launch(SessionKind::Claude, "claude --model opus");
        manager.prepare_browser_launch_for_session(
            &mut replacement,
            session_id,
            crate::browser::BrowserWorkspaceSnapshot::default(),
        );
        let (replacement_token, replacement_overlay, replacement_workspace) = {
            let sessions = manager.inner.browser_provider_sessions.lock().unwrap();
            let replacement = sessions.get(session_id).unwrap();
            (
                replacement
                    .registration
                    .access()
                    .bearer_token_for_launch()
                    .to_string(),
                replacement
                    ._claude_overlay
                    .as_ref()
                    .unwrap()
                    .path()
                    .to_path_buf(),
                replacement.registration.workspace_key().clone(),
            )
        };
        assert!(!old_overlay.exists());
        assert!(replacement_overlay.exists());

        let coordinator = bridge.replay_coordinator();
        let replacement_replay = coordinator
            .start(
                replacement_workspace.clone(),
                browser_provider_replay_plan("replacement", true),
            )
            .unwrap();
        let (mut prompt, _) = BrowserReplaySecretPromptVault::install(
            replacement_replay.instance.clone(),
            vec!["password".to_string()],
        )
        .unwrap();
        prompt
            .edit(
                &replacement_replay.instance,
                "password",
                "replacement-provider-secret",
            )
            .unwrap();
        let (submission, _) = prompt.submit(&replacement_replay.instance).unwrap();
        coordinator
            .submit_secrets(&replacement_replay.instance, submission)
            .unwrap();
        let secret_lease = replacement_replay
            .execution
            .secret_lease("password")
            .unwrap();
        let isolated_workspace =
            BrowserWorkspaceKey::new("browser-project", "sibling-conversation").unwrap();
        let isolated = coordinator
            .start(
                isolated_workspace,
                browser_provider_replay_plan("isolated", false),
            )
            .unwrap();
        let controller = bridge.bind(replacement_workspace.clone(), Duration::from_secs(1));
        let pending = tokio::spawn(async move {
            controller
                .request(BrowserCommand::Reload {
                    tab_id: "runtime-tab".to_string(),
                })
                .await
        });
        let late_request = inbox.recv().await.expect("retained replacement request");

        manager.update_session_state(session_id, |state| {
            state.status = SessionStatus::Exited;
        });
        old_exit_notifier();

        assert_eq!(gateway.registrar().active_registration_count(), 1);
        assert_eq!(
            manager
                .inner
                .browser_provider_sessions
                .lock()
                .unwrap()
                .get(session_id)
                .unwrap()
                .registration
                .access()
                .bearer_token_for_launch(),
            replacement_token
        );
        assert!(replacement_overlay.exists());
        assert_eq!(
            coordinator
                .status(&replacement_replay.instance)
                .unwrap()
                .status,
            BrowserReplayStatus::Running,
            "an old exit callback must not cancel replacement replay authority"
        );
        assert!(!pending.is_finished());

        let replacement_exit_notifier =
            session_change_notifier(manager.inner.clone(), session_id.to_string());
        replacement_exit_notifier();

        assert!(!manager
            .inner
            .browser_provider_sessions
            .lock()
            .unwrap()
            .contains_key(session_id));
        assert_eq!(gateway.registrar().active_registration_count(), 0);
        assert!(!replacement_overlay.exists());
        assert_eq!(
            coordinator
                .status(&replacement_replay.instance)
                .unwrap()
                .status,
            BrowserReplayStatus::Cancelled
        );
        assert_eq!(
            secret_lease.expose(|_| ()),
            Err(BrowserReplaySecretError::ClosedStore)
        );
        late_request.respond(Ok(BrowserResponse::Acknowledged));
        assert_eq!(pending.await.unwrap(), Err(BrowserError::Interrupted));
        assert_eq!(
            coordinator
                .status(&replacement_replay.instance)
                .unwrap()
                .current_step_index,
            0
        );
        assert_eq!(
            coordinator.status(&isolated.instance).unwrap().status,
            BrowserReplayStatus::Pending
        );
    }

    #[test]
    fn codex_hooks_launch_carries_browser_config_and_no_remote() {
        let (bridge, _inbox) = crate::browser::browser_command_channel(8);
        let gateway = crate::browser::BrowserGatewayHandle::start(bridge).unwrap();
        let manager = ProcessManager::new();
        manager.set_browser_gateway_registrar(Some(gateway.registrar()));
        manager.set_codex_hooks_support_probe_for_test(Arc::new(|_| Ok(())));
        let mut launch = browser_test_launch(SessionKind::Codex, "codex --full-auto");
        manager.prepare_browser_launch_for_session(
            &mut launch,
            "codex-hooks-browser",
            crate::browser::BrowserWorkspaceSnapshot::default(),
        );
        let token = manager.browser_environment("codex-hooks-browser")
            [crate::browser::DEVMANAGER_BROWSER_TOKEN_ENV]
            .clone();

        let terminal_environment =
            manager.prepare_codex_launch_for_session(&mut launch, "codex-hooks-browser");
        assert_eq!(
            terminal_environment.get(crate::browser::DEVMANAGER_BROWSER_TOKEN_ENV),
            Some(&token)
        );
        assert!(launch.startup_command.contains(
            "mcp_servers.devmanager_browser.bearer_token_env_var=\"DEVMANAGER_BROWSER_TOKEN\""
        ));
        assert!(
            launch.startup_command.contains("codex --full-auto")
                || launch.startup_command.contains("'codex' '--full-auto'")
        );
        assert!(!launch.startup_command.contains("--remote"));
        assert!(launch.startup_command.contains("codex-hook-relay"));
        assert!(launch
            .startup_command
            .contains("--dangerously-bypass-hook-trust"));
        for event in crate::ai::codex_hooks::CODEX_HOOK_EVENTS {
            assert!(
                launch.startup_command.contains(&format!("hooks.{event}=")),
                "missing {event} in {}",
                launch.startup_command
            );
        }
    }

    #[test]
    fn codex_session_start_binds_hook_identity_and_reports_healthy() {
        let manager = ProcessManager::new();
        manager.set_codex_hooks_support_probe_for_test(Arc::new(|_| Ok(())));
        let events: Arc<Mutex<Vec<RemoteSessionEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            sink.lock().unwrap().push(event);
        })));
        let mut launch = browser_test_launch(SessionKind::Codex, "codex --full-auto");
        let session_id = "codex-hooks-session-start";
        manager.prepare_codex_launch_for_session(&mut launch, session_id);
        let registration = match manager
            .inner
            .codex_adapter_registry
            .lock()
            .unwrap()
            .sessions
            .get(session_id)
            .expect("session installed")
        {
            CodexAdapterSession::Running { registration, .. } => registration.clone(),
            other => panic!("expected running codex session, got {other:?}"),
        };

        let body = serde_json::json!({
            "session_id": "prov-1",
            "cwd": "C:\\proj",
            "transcript_path": "C:\\ignored\\rollout.jsonl",
            "hook_event_name": "SessionStart"
        })
        .to_string();
        let status = manager.inner.codex_hook_registry.ingest(
            "127.0.0.1:9999".parse().unwrap(),
            &registration.nonce,
            body.as_bytes(),
            1,
        );
        assert_eq!(
            status,
            crate::ai::codex_hooks::CodexRelayIngestStatus::Accepted
        );

        assert!(matches!(
            manager
                .inner
                .codex_adapter_registry
                .lock()
                .unwrap()
                .sessions
                .get(session_id),
            Some(CodexAdapterSession::Running {
                activated: true,
                ..
            })
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            {
                let events = events.lock().unwrap();
                let healthy = events.iter().any(|event| {
                    matches!(
                        event,
                        RemoteSessionEvent::AdapterHealth {
                            health: SemanticAdapterHealth::Healthy,
                            ..
                        }
                    )
                });
                let ready = events.iter().any(|event| {
                    matches!(
                        event,
                        RemoteSessionEvent::CodexSemantic { draft, .. }
                            if matches!(
                                &draft.kind,
                                crate::remote::presentation::SemanticEventKind::Status {
                                    state,
                                    ..
                                } if state == "ready"
                            )
                    )
                });
                assert!(
                    ready,
                    "SessionStart should publish only its hook-derived ready status"
                );
                if healthy {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "healthy adapter event never arrived"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn codex_close_unregisters_relay_nonce() {
        let manager = ProcessManager::new();
        manager.set_codex_hooks_support_probe_for_test(Arc::new(|_| Ok(())));
        let mut launch = browser_test_launch(SessionKind::Codex, "codex --full-auto");
        let session_id = "codex-hooks-close";
        manager.prepare_codex_launch_for_session(&mut launch, session_id);
        let registration = match manager
            .inner
            .codex_adapter_registry
            .lock()
            .unwrap()
            .sessions
            .get(session_id)
            .expect("session installed")
        {
            CodexAdapterSession::Running { registration, .. } => registration.clone(),
            other => panic!("expected running codex session, got {other:?}"),
        };

        manager.cleanup_codex_adapter_session(session_id);

        let body = serde_json::json!({
            "session_id": "prov-1", "cwd": "C:\\proj", "transcript_path": null,
            "hook_event_name": "Stop"
        })
        .to_string();
        assert_eq!(
            manager.inner.codex_hook_registry.ingest(
                "127.0.0.1:9999".parse().unwrap(),
                &registration.nonce,
                body.as_bytes(),
                1,
            ),
            crate::ai::codex_hooks::CodexRelayIngestStatus::Rejected
        );
    }

    #[test]
    fn codex_preparer_failure_revokes_browser_and_preserves_original_launch() {
        let (bridge, _inbox) = crate::browser::browser_command_channel(8);
        let gateway = crate::browser::BrowserGatewayHandle::start(bridge).unwrap();
        let manager = ProcessManager::new();
        manager.set_browser_gateway_registrar(Some(gateway.registrar()));
        manager.set_codex_hooks_support_probe_for_test(Arc::new(|_| {
            Err("fixture probe failed".to_string())
        }));
        let mut launch = browser_test_launch(SessionKind::Codex, "codex --full-auto");
        let original = launch.clone();
        manager.prepare_browser_launch_for_session(
            &mut launch,
            "codex-browser-preparer-failure",
            crate::browser::BrowserWorkspaceSnapshot::default(),
        );

        let environment =
            manager.prepare_codex_launch_for_session(&mut launch, "codex-browser-preparer-failure");

        assert!(environment.is_empty());
        assert_eq!(launch.startup_command, original.startup_command);
        assert_eq!(gateway.registrar().active_registration_count(), 0);
        assert!(manager
            .browser_diagnostic(&launch.tab_id)
            .unwrap()
            .contains("Codex launch preparation failed"));
    }

    #[test]
    fn codex_preparer_failure_does_not_leak_revoked_browser_env_to_terminal_spawn() {
        let (bridge, _inbox) = crate::browser::browser_command_channel(8);
        let gateway = crate::browser::BrowserGatewayHandle::start(bridge).unwrap();
        let manager = ProcessManager::new();
        manager.set_browser_gateway_registrar(Some(gateway.registrar()));
        manager.set_codex_hooks_support_probe_for_test(Arc::new(|_| {
            Err("fixture probe failed".to_string())
        }));
        let mut launch = browser_test_launch(SessionKind::Codex, "codex --full-auto");
        manager.prepare_browser_launch_for_session(
            &mut launch,
            "codex-browser-spawn-failure",
            crate::browser::BrowserWorkspaceSnapshot::default(),
        );

        let environment =
            manager.prepare_ai_terminal_environment(&mut launch, "codex-browser-spawn-failure");

        assert!(environment.is_empty());
        assert_eq!(gateway.registrar().active_registration_count(), 0);
    }

    #[test]
    fn startup_command_write_failure_cleans_session_and_browser_credentials() {
        let (bridge, _inbox) = crate::browser::browser_command_channel(8);
        let gateway = crate::browser::BrowserGatewayHandle::start(bridge).unwrap();
        let manager = ProcessManager::new();
        manager.set_browser_gateway_registrar(Some(gateway.registrar()));
        let session_id = "startup-write-failure";
        let mut launch = browser_test_launch(SessionKind::Claude, "claude --model sonnet");
        if !cfg!(windows) {
            launch.shell_program = "/bin/sh".to_string();
        }
        manager.prepare_browser_launch_for_session(
            &mut launch,
            session_id,
            crate::browser::BrowserWorkspaceSnapshot::default(),
        );
        let overlay_path = manager
            .inner
            .browser_provider_sessions
            .lock()
            .unwrap()
            .get(session_id)
            .unwrap()
            ._claude_overlay
            .as_ref()
            .unwrap()
            .path()
            .to_path_buf();
        manager.ensure_runtime_entry(session_id, launch.cwd.clone(), SessionDimensions::default());

        let result = spawn_ai_session_with_writer(
            &manager.inner,
            &launch,
            session_id,
            SessionDimensions::default(),
            |_session, _command| Err("fixture PTY write failed".to_string()),
        );

        let error = result.expect_err("startup command write failure must fail the spawn");
        assert!(error.contains("fixture PTY write failed"));
        assert!(!manager.session_exists(session_id));
        assert!(!manager
            .inner
            .browser_provider_sessions
            .lock()
            .unwrap()
            .contains_key(session_id));
        assert_eq!(gateway.registrar().active_registration_count(), 0);
        assert!(!overlay_path.exists());
        assert_eq!(
            manager.runtime_state().sessions[session_id].status,
            SessionStatus::Failed
        );
        assert!(manager
            .browser_attachment_broker()
            .binding(session_id)
            .is_none());
    }

    #[test]
    fn pty_spawn_failure_unbinds_its_captured_attachment_generation() {
        let manager = ProcessManager::new();
        let session_id = "attachment-pty-spawn-failure";
        let mut launch = browser_test_launch(SessionKind::Claude, "claude --model sonnet");
        launch.shell_program = "definitely-not-a-devmanager-shell".to_string();
        let binding = manager
            .prepare_browser_launch_for_session(
                &mut launch,
                session_id,
                browser_attachment_snapshot("ann-spawn-failure"),
            )
            .unwrap();
        manager.ensure_runtime_entry(session_id, launch.cwd.clone(), SessionDimensions::default());

        let error = spawn_ai_session_with_attachment_binding(
            &manager.inner,
            &launch,
            session_id,
            SessionDimensions::default(),
            Some(binding),
        )
        .expect_err("invalid shell must fail PTY spawn");

        assert!(!error.is_empty());
        assert!(manager
            .browser_attachment_broker()
            .binding(session_id)
            .is_none());
    }

    #[test]
    fn output_notifier_forwards_the_native_terminal_mode() {
        let manager = ProcessManager::new();
        let (tx, rx) = std::sync::mpsc::channel();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            if let RemoteSessionEvent::Output { mode, .. } = event {
                tx.send(mode).expect("mode receiver should remain open");
            }
        })));
        let notifier = session_output_notifier(manager.inner.clone(), "alpha".to_string());
        let mode = crate::terminal::session::TerminalModeSnapshot {
            alternate_screen: true,
            mouse_report_click: true,
            ..crate::terminal::session::TerminalModeSnapshot::default()
        };

        notifier(b"output".to_vec(), mode);

        assert_eq!(rx.recv_timeout(Duration::from_millis(100)), Ok(mode));
    }

    #[test]
    fn output_notifier_attaches_screen_snapshot_only_for_ai_sessions() {
        let manager = ProcessManager::new();
        let cwd = std::env::current_dir().unwrap();
        manager
            .spawn_shell_session("ai-session", &cwd, SessionDimensions::default(), None, None)
            .expect("ai session");
        manager
            .spawn_shell_session(
                "server-session",
                &cwd,
                SessionDimensions::default(),
                None,
                None,
            )
            .expect("server session");

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let runtime = manager.inner.runtime_state.read().unwrap();
            if runtime.sessions.contains_key("ai-session")
                && runtime.sessions.contains_key("server-session")
            {
                break;
            }
            drop(runtime);
            thread::sleep(Duration::from_millis(10));
        }

        {
            let mut runtime = manager.inner.runtime_state.write().unwrap();
            runtime
                .sessions
                .get_mut("ai-session")
                .expect("ai runtime")
                .session_kind = SessionKind::Claude;
            runtime
                .sessions
                .get_mut("server-session")
                .expect("server runtime")
                .session_kind = SessionKind::Server;
        }
        manager
            .inner
            .sessions
            .lock()
            .unwrap()
            .get("ai-session")
            .expect("ai terminal")
            .write_virtual_text("assistant visible text");

        assert!(
            ai_session_screen_snapshot(&manager.inner, "ai-session").is_some(),
            "Claude sessions must attach a post-parse screen snapshot"
        );
        assert!(
            ai_session_screen_snapshot(&manager.inner, "server-session").is_none(),
            "non-AI sessions must not pay the snapshot cost"
        );

        let (tx, rx) = std::sync::mpsc::channel();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            if let RemoteSessionEvent::Output {
                session_id,
                screen,
                bytes,
                ..
            } = event
            {
                if bytes == b"probe-chunk" {
                    let _ = tx.send((session_id, screen.is_some()));
                }
            }
        })));

        let ai_notifier = session_output_notifier(manager.inner.clone(), "ai-session".to_string());
        let server_notifier =
            session_output_notifier(manager.inner.clone(), "server-session".to_string());
        let mode = TerminalModeSnapshot::default();
        ai_notifier(b"probe-chunk".to_vec(), mode);
        server_notifier(b"probe-chunk".to_vec(), mode);

        let first = rx.recv_timeout(Duration::from_secs(1)).expect("ai event");
        let second = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("server event");
        let events = [first, second];
        assert!(events.contains(&("ai-session".to_string(), true)));
        assert!(events.contains(&("server-session".to_string(), false)));

        let _ = manager.close_session("ai-session");
        let _ = manager.close_session("server-session");
    }

    #[test]
    fn remote_event_callbacks_can_replace_the_handler_without_deadlocking() {
        let manager = ProcessManager::new();
        let callback_manager = manager.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        manager.set_remote_session_handler(Some(Arc::new(move |_| {
            callback_manager.set_remote_session_handler(None);
            tx.send(()).unwrap();
        })));
        let notifier = session_output_notifier(manager.inner.clone(), "lock-test".to_string());

        thread::spawn(move || {
            notifier(
                b"output".to_vec(),
                crate::terminal::session::TerminalModeSnapshot::default(),
            );
        });

        assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok(()));
    }

    #[test]
    fn codex_preparation_failure_is_fail_open_and_marks_adapter_degraded() {
        let manager = ProcessManager::new();
        manager.set_codex_hooks_support_probe_for_test(Arc::new(|_| {
            Err("capability unavailable".to_string())
        }));
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        })));
        let mut launch = AiLaunchSpec {
            tab_id: "codex-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Codex,
            cwd: std::env::current_dir().unwrap(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "my-codex-wrapper --custom".to_string(),
        };

        manager.prepare_codex_launch_for_session(&mut launch, "codex-session");

        assert_eq!(launch.startup_command, "my-codex-wrapper --custom");
        assert!(matches!(
            manager
                .inner
                .codex_adapter_registry
                .lock()
                .unwrap()
                .sessions
                .get("codex-session"),
            Some(CodexAdapterSession::Degraded(_))
        ));
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::AdapterHealth {
                stable_session_key,
                health: SemanticAdapterHealth::Degraded,
            } if stable_session_key == &StableSessionKey::from_tab("codex-tab")
        )));
        manager.cleanup_codex_adapter_session("codex-session");
        assert!(manager
            .inner
            .codex_adapter_registry
            .lock()
            .unwrap()
            .sessions
            .is_empty());
    }

    #[test]
    fn codex_generation_exhaustion_fails_closed_without_wrapping_or_adapting() {
        let manager = ProcessManager::new();
        manager
            .inner
            .codex_adapter_generation
            .store(u64::MAX, Ordering::Relaxed);
        let prepare_calls = Arc::new(AtomicU64::new(0));
        let observed_calls = prepare_calls.clone();
        manager.set_codex_hooks_support_probe_for_test(Arc::new(move |_| {
            observed_calls.fetch_add(1, Ordering::Relaxed);
            Err("must not probe after generation exhaustion".to_string())
        }));
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        })));
        let mut launch = AiLaunchSpec {
            tab_id: "codex-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Codex,
            cwd: std::env::current_dir().unwrap(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "codex --full-auto".to_string(),
        };

        manager.prepare_codex_launch_for_session(&mut launch, "codex-session");

        assert_eq!(launch.startup_command, "codex --full-auto");
        assert_eq!(prepare_calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            manager
                .inner
                .codex_adapter_generation
                .load(Ordering::Relaxed),
            u64::MAX
        );
        assert!(manager
            .inner
            .codex_adapter_registry
            .lock()
            .unwrap()
            .sessions
            .is_empty());
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::AdapterHealth {
                stable_session_key,
                health: SemanticAdapterHealth::Degraded,
            } if stable_session_key == &StableSessionKey::from_tab("codex-tab")
        )));
    }

    #[test]
    fn codex_publication_revalidates_latest_session_generation() {
        use crate::remote::presentation::{SemanticEventKind, SemanticRetention, SemanticSource};

        let manager = ProcessManager::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        })));
        let stable_session_key = StableSessionKey::from_tab("codex-tab");
        let old = CodexAdapterIdentity {
            stable_session_key: stable_session_key.clone(),
            generation: 1,
        };
        let current = CodexAdapterIdentity {
            stable_session_key: stable_session_key.clone(),
            generation: 2,
        };
        {
            let mut registry = manager.inner.codex_adapter_registry.lock().unwrap();
            registry.note_generation(&old);
            registry.note_generation(&current);
            registry
                .sessions
                .insert("old".to_string(), CodexAdapterSession::Pending(old.clone()));
            registry.sessions.insert(
                "current".to_string(),
                CodexAdapterSession::Pending(current.clone()),
            );
        }
        let draft = |detail: &str| SemanticEventDraft {
            stable_session_key: stable_session_key.clone(),
            occurred_at_epoch_ms: 1,
            source: SemanticSource::Codex,
            kind: SemanticEventKind::Status {
                state: "idle".to_string(),
                detail: Some(detail.to_string()),
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: None,
        };

        emit_codex_semantic_if_current(&manager.inner, "old", &old, draft("old"));
        emit_codex_semantic_if_current(&manager.inner, "current", &current, draft("current"));
        emit_codex_health_if_current(&manager.inner, &old, SemanticAdapterHealth::Degraded);

        let events = events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RemoteSessionEvent::CodexSemantic { .. }))
                .count(),
            1
        );
        assert!(events.iter().any(|event| matches!(
            event,
            RemoteSessionEvent::CodexSemantic { identity, draft }
                if identity == &codex_semantic_identity("current", &current) && matches!(
                &draft.kind,
                SemanticEventKind::Status { detail: Some(detail), .. } if detail == "current"
            )
        )));
        assert!(!events
            .iter()
            .any(|event| matches!(event, RemoteSessionEvent::AdapterHealth { .. })));
    }

    #[test]
    fn codex_old_generation_cannot_resume_after_newer_session_cleanup() {
        use crate::remote::presentation::{SemanticEventKind, SemanticRetention, SemanticSource};

        let manager = ProcessManager::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        })));
        let stable_session_key = StableSessionKey::from_tab("codex-tab");
        let old = CodexAdapterIdentity {
            stable_session_key: stable_session_key.clone(),
            generation: 1,
        };
        let current = CodexAdapterIdentity {
            stable_session_key: stable_session_key.clone(),
            generation: 2,
        };
        {
            let mut registry = manager.inner.codex_adapter_registry.lock().unwrap();
            registry.note_generation(&old);
            registry.note_generation(&current);
            registry
                .sessions
                .insert("old".to_string(), CodexAdapterSession::Pending(old.clone()));
            registry.sessions.insert(
                "current".to_string(),
                CodexAdapterSession::Pending(current.clone()),
            );
        }

        assert!(cleanup_codex_adapter_session_if_matches(
            &manager.inner,
            "current",
            &current,
        ));
        emit_codex_semantic_if_current(
            &manager.inner,
            "old",
            &old,
            SemanticEventDraft {
                stable_session_key,
                occurred_at_epoch_ms: 1,
                source: SemanticSource::Codex,
                kind: SemanticEventKind::Status {
                    state: "idle".to_string(),
                    detail: Some("stale".to_string()),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: None,
            },
        );

        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn claude_launch_preparation_is_private_and_cleanup_is_session_scoped() {
        let temp = temp_test_dir("claude-hook-launch");
        let manager = ProcessManager::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        })));
        let mut launch = AiLaunchSpec {
            tab_id: "claude-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude --model sonnet".to_string(),
        };

        manager.prepare_claude_launch_for_session(&mut launch, "claude-session", &temp);

        assert!(launch.startup_command.contains("--settings"));
        assert_eq!(manager.inner.claude_hook_registry.registration_count(), 1);
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::ClaudeAdapterRegistered { identity }
                if identity.pty_session_id == "claude-session"
                    && identity.stable_session_key == crate::remote::presentation::StableSessionKey::from_tab("claude-tab")
        )));
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::AdapterHealth {
                stable_session_key,
                health: crate::remote::presentation::SemanticAdapterHealth::Degraded,
            } if stable_session_key == &crate::remote::presentation::StableSessionKey::from_tab("claude-tab")
        )));
        let (registration, settings_path) = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("claude-session")
            .map(|session| (session.registration.clone(), session.settings_path.clone()))
            .expect("Claude hook session");
        assert!(settings_path.is_file());
        assert!(!settings_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&registration.nonce));

        events.lock().unwrap().clear();
        let endpoint = manager.claude_hook_endpoint().unwrap();
        ureq::post(&endpoint)
            .header("x-devmanager-claude-nonce", &registration.nonce)
            .send(br#"{"hook_event_name":"SessionStart","session_id":"provider-session","source":"startup"}"#)
            .unwrap();
        let started_at = Instant::now();
        while started_at.elapsed() < Duration::from_secs(2)
            && !events.lock().unwrap().iter().any(|event| matches!(
                event,
                RemoteSessionEvent::AdapterHealth {
                    stable_session_key,
                    health: crate::remote::presentation::SemanticAdapterHealth::Healthy,
                } if stable_session_key == &crate::remote::presentation::StableSessionKey::from_tab("claude-tab")
            ))
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::AdapterHealth {
                stable_session_key,
                health: crate::remote::presentation::SemanticAdapterHealth::Healthy,
            } if stable_session_key == &crate::remote::presentation::StableSessionKey::from_tab("claude-tab")
        )));
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::ClaudeSemantic { identity, draft }
                if identity.pty_session_id == "claude-session"
                    && matches!(&draft.kind, crate::remote::presentation::SemanticEventKind::Status { state, .. } if state == "started")
        )));

        manager.cleanup_claude_hook_session("claude-session");

        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::ClaudeAdapterRemoved { identity }
                if identity.pty_session_id == "claude-session"
        )));

        assert_eq!(manager.inner.claude_hook_registry.registration_count(), 0);
        assert!(!settings_path.exists());
    }

    #[test]
    fn claude_cleanup_fences_hook_publication_before_losing_identity_correlation() {
        let temp = temp_test_dir("claude-hook-cleanup-publication-fence");
        let manager = ProcessManager::new();
        let mut launch = AiLaunchSpec {
            tab_id: "claude-fence-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut launch, "claude-fence-session", &temp);
        let registration = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("claude-fence-session")
            .expect("Claude hook session")
            .registration
            .clone();
        let endpoint = manager.claude_hook_endpoint().unwrap();
        ureq::post(&endpoint)
            .header("x-devmanager-claude-nonce", &registration.nonce)
            .send(
                br#"{"hook_event_name":"SessionStart","session_id":"provider-fence","source":"startup"}"#,
            )
            .unwrap();

        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        let cleanup_gate = Arc::new((Mutex::new((false, false, false)), Condvar::new()));
        let handler_gate = cleanup_gate.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            if matches!(
                &event,
                RemoteSessionEvent::Semantic { draft }
                    if matches!(
                        &draft.kind,
                        crate::remote::presentation::SemanticEventKind::UserMessage { text }
                            if text == "racing prompt"
                    )
            ) {
                let (lock, condition) = &*handler_gate;
                let mut state = lock.lock().unwrap();
                state.2 = true;
                condition.notify_all();
            }
            if matches!(event, RemoteSessionEvent::ClaudeAdapterRemoved { .. }) {
                let (lock, condition) = &*handler_gate;
                let mut state = lock.lock().unwrap();
                state.0 = true;
                condition.notify_all();
                while !state.1 {
                    state = condition.wait(state).unwrap();
                }
            }
            observed.lock().unwrap().push(event);
        })));

        let cleanup_manager = manager.clone();
        let cleanup = thread::spawn(move || {
            cleanup_manager.cleanup_claude_hook_session("claude-fence-session");
        });
        {
            let (lock, condition) = &*cleanup_gate;
            let state = lock.lock().unwrap();
            let (state, timeout) = condition
                .wait_timeout_while(state, Duration::from_secs(2), |state| !state.0)
                .unwrap();
            assert!(!timeout.timed_out(), "cleanup reached adapter removal");
            drop(state);
        }

        let _ = ureq::post(&endpoint)
            .header("x-devmanager-claude-nonce", &registration.nonce)
            .send(
                br#"{"hook_event_name":"UserPromptSubmit","session_id":"provider-fence","prompt":"racing prompt"}"#,
            );

        let generic_escaped = {
            let (lock, condition) = &*cleanup_gate;
            let state = lock.lock().unwrap();
            let (state, _) = condition
                .wait_timeout_while(state, Duration::from_secs(2), |state| !state.2)
                .unwrap();
            state.2
        };

        {
            let (lock, condition) = &*cleanup_gate;
            let mut state = lock.lock().unwrap();
            state.1 = true;
            condition.notify_all();
        }
        cleanup.join().unwrap();

        assert!(
            !generic_escaped,
            "cleanup must not let a current hook bypass Claude identity reconciliation"
        );
    }

    #[test]
    fn claude_cleanup_preserves_identity_for_an_admitted_hook_until_publication_finishes() {
        let temp = temp_test_dir("claude-hook-admitted-publication-fence");
        let manager = ProcessManager::new();
        let mut launch = AiLaunchSpec {
            tab_id: "claude-admitted-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut launch, "claude-admitted-session", &temp);
        let registration = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("claude-admitted-session")
            .expect("Claude hook session")
            .registration
            .clone();
        let endpoint = manager.claude_hook_endpoint().unwrap();
        ureq::post(&endpoint)
            .header("x-devmanager-claude-nonce", &registration.nonce)
            .send(
                br#"{"hook_event_name":"SessionStart","session_id":"provider-admitted","source":"startup"}"#,
            )
            .unwrap();

        let publication_gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let hook_gate = publication_gate.clone();
        *manager
            .inner
            .claude_semantic_publication_test_hook
            .write()
            .unwrap() = Some(Arc::new(move || {
            let (lock, condition) = &*hook_gate;
            let mut state = lock.lock().unwrap();
            state.0 = true;
            condition.notify_all();
            while !state.1 {
                state = condition.wait(state).unwrap();
            }
        }));

        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        let (removed_tx, removed_rx) = std::sync::mpsc::channel();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            if matches!(event, RemoteSessionEvent::ClaudeAdapterRemoved { .. }) {
                let _ = removed_tx.send(());
            }
            observed.lock().unwrap().push(event);
        })));

        ureq::post(&endpoint)
            .header("x-devmanager-claude-nonce", &registration.nonce)
            .send(
                br#"{"hook_event_name":"UserPromptSubmit","session_id":"provider-admitted","prompt":"admitted prompt"}"#,
            )
            .unwrap();
        {
            let (lock, condition) = &*publication_gate;
            let state = lock.lock().unwrap();
            let (state, timeout) = condition
                .wait_timeout_while(state, Duration::from_secs(2), |state| !state.0)
                .unwrap();
            assert!(!timeout.timed_out(), "hook reached validated publication");
            drop(state);
        }

        let cleanup_manager = manager.clone();
        let (cleanup_started_tx, cleanup_started_rx) = std::sync::mpsc::channel();
        let cleanup = thread::spawn(move || {
            cleanup_started_tx.send(()).unwrap();
            cleanup_manager.cleanup_claude_hook_session("claude-admitted-session");
        });
        cleanup_started_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let removed_before_publication_finished =
            removed_rx.recv_timeout(Duration::from_secs(2)).is_ok();

        {
            let (lock, condition) = &*publication_gate;
            let mut state = lock.lock().unwrap();
            state.1 = true;
            condition.notify_all();
        }
        cleanup.join().unwrap();

        let events = events.lock().unwrap();
        assert!(
            !removed_before_publication_finished,
            "adapter removal must wait for admitted publication to finish"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            RemoteSessionEvent::ClaudeSemantic { identity, draft }
                if identity.pty_session_id == "claude-admitted-session"
                    && identity.registration_generation == registration.generation
                    && matches!(
                        &draft.kind,
                        crate::remote::presentation::SemanticEventKind::UserMessage { text }
                            if text == "admitted prompt"
                    )
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            RemoteSessionEvent::Semantic { draft }
                if matches!(
                    &draft.kind,
                    crate::remote::presentation::SemanticEventKind::UserMessage { text }
                        if text == "admitted prompt"
                )
        )));
    }

    #[test]
    fn logical_session_end_survives_until_exact_pty_generation_exit() {
        let temp = temp_test_dir("claude-hook-replacement");
        let manager = ProcessManager::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        })));
        let mut old_launch = AiLaunchSpec {
            tab_id: "shared-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut old_launch, "old-session", &temp);
        manager.ensure_runtime_entry("old-session", temp.clone(), SessionDimensions::default());
        manager.update_session_state("old-session", |state| {
            state.status = SessionStatus::Running;
        });
        let old_exit_notifier =
            session_change_notifier(manager.inner.clone(), "old-session".to_string());
        let (old_registration, old_settings_path) = {
            let sessions = manager.inner.claude_hook_sessions.lock().unwrap();
            let old = sessions.get("old-session").unwrap();
            (old.registration.clone(), old.settings_path.clone())
        };
        let endpoint = manager.claude_hook_endpoint().unwrap();
        ureq::post(&endpoint)
            .header("x-devmanager-claude-nonce", &old_registration.nonce)
            .send(
                br#"{"hook_event_name":"SessionStart","session_id":"provider-old","source":"startup"}"#,
            )
            .unwrap();
        let response = ureq::post(&endpoint)
            .header("x-devmanager-claude-nonce", &old_registration.nonce)
            .send(
                br#"{"hook_event_name":"SessionEnd","session_id":"provider-old","reason":"clear"}"#,
            )
            .unwrap();
        let mut replacement = old_launch.clone();
        replacement.startup_command = "claude".to_string();
        manager.prepare_claude_launch_for_session(&mut replacement, "new-session", &temp);
        let new_settings_path = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("new-session")
            .map(|session| session.settings_path.clone())
            .unwrap();
        events.lock().unwrap().clear();

        assert_eq!(response.status().as_u16(), 204);
        assert!(manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .contains_key("old-session"));
        assert_eq!(manager.inner.claude_hook_registry.registration_count(), 2);
        assert!(old_settings_path.exists());
        assert!(new_settings_path.exists());
        assert!(!events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::AdapterHealth {
                stable_session_key,
                health: crate::remote::presentation::SemanticAdapterHealth::Degraded,
            } if stable_session_key == &crate::remote::presentation::StableSessionKey::from_tab("shared-tab")
        )));

        manager.update_session_state("old-session", |state| {
            state.status = SessionStatus::Exited;
        });
        old_exit_notifier();

        assert!(!manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .contains_key("old-session"));
        assert_eq!(manager.inner.claude_hook_registry.registration_count(), 1);
        assert!(!old_settings_path.exists());
        assert!(new_settings_path.exists());
        manager.cleanup_claude_hook_session("new-session");
    }

    #[test]
    fn late_old_pty_exit_cannot_remove_replacement_for_reused_session_id() {
        let temp = temp_test_dir("claude-hook-reused-session");
        let manager = ProcessManager::new();
        let mut launch = AiLaunchSpec {
            tab_id: "shared-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut launch, "shared-session", &temp);
        manager.ensure_runtime_entry("shared-session", temp.clone(), SessionDimensions::default());
        manager.update_session_state("shared-session", |state| {
            state.status = SessionStatus::Running;
        });
        let old_exit_notifier =
            session_change_notifier(manager.inner.clone(), "shared-session".to_string());
        let old_generation = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("shared-session")
            .unwrap()
            .registration
            .generation;

        launch.startup_command = "claude".to_string();
        manager.prepare_claude_launch_for_session(&mut launch, "shared-session", &temp);
        let (replacement_generation, replacement_path) = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("shared-session")
            .map(|session| {
                (
                    session.registration.generation,
                    session.settings_path.clone(),
                )
            })
            .unwrap();
        assert!(replacement_generation > old_generation);

        manager.update_session_state("shared-session", |state| {
            state.status = SessionStatus::Exited;
        });
        old_exit_notifier();

        let sessions = manager.inner.claude_hook_sessions.lock().unwrap();
        assert_eq!(
            sessions
                .get("shared-session")
                .unwrap()
                .registration
                .generation,
            replacement_generation
        );
        drop(sessions);
        assert!(replacement_path.exists());
        manager.cleanup_claude_hook_session("shared-session");
    }

    #[test]
    fn unexpected_pty_exit_without_session_end_cleans_registration() {
        let temp = temp_test_dir("claude-hook-unexpected-exit");
        let manager = ProcessManager::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        })));
        let mut launch = AiLaunchSpec {
            tab_id: "unexpected-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut launch, "unexpected-session", &temp);
        manager.ensure_runtime_entry(
            "unexpected-session",
            temp.clone(),
            SessionDimensions::default(),
        );
        manager.update_session_state("unexpected-session", |state| {
            state.status = SessionStatus::Running;
        });
        let exit_notifier =
            session_change_notifier(manager.inner.clone(), "unexpected-session".to_string());
        let settings_path = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("unexpected-session")
            .unwrap()
            .settings_path
            .clone();
        events.lock().unwrap().clear();

        manager.update_session_state("unexpected-session", |state| {
            state.status = SessionStatus::Crashed;
        });
        exit_notifier();

        assert_eq!(manager.inner.claude_hook_registry.registration_count(), 0);
        assert!(!manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .contains_key("unexpected-session"));
        assert!(!settings_path.exists());
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::ClaudeAdapterRemoved { identity }
                if identity.pty_session_id == "unexpected-session"
        )));
    }

    #[test]
    fn expired_claude_registration_degrades_the_exact_session_and_cleans_tracking() {
        let temp = temp_test_dir("claude-hook-expiry");
        let manager = ProcessManager::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        })));
        let mut launch = AiLaunchSpec {
            tab_id: "expiring-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut launch, "expiring-session", &temp);
        events.lock().unwrap().clear();

        let removed = manager
            .inner
            .claude_hook_registry
            .cleanup_expired_at(Instant::now() + Duration::from_secs(6 * 60));

        assert_eq!(removed, 1);
        assert!(!manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .contains_key("expiring-session"));
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::AdapterHealth {
                stable_session_key,
                health: crate::remote::presentation::SemanticAdapterHealth::Degraded,
            } if stable_session_key == &crate::remote::presentation::StableSessionKey::from_tab("expiring-tab")
        )));
    }

    #[test]
    fn claude_spawn_failure_immediately_removes_registration_and_settings() {
        let temp = temp_test_dir("claude-hook-spawn-failure");
        let _pid_file_guard = pid_file::use_test_pid_file(temp.join("running-pids.json"));
        let manager = ProcessManager::new();
        let mut launch = AiLaunchSpec {
            tab_id: "failure-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut launch, "failure-session", &temp);
        let settings_path = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("failure-session")
            .map(|session| session.settings_path.clone())
            .unwrap();
        launch.shell_program = "definitely-missing-devmanager-shell".to_string();

        let result = spawn_ai_session_with_inner(
            &manager.inner,
            &launch,
            "failure-session",
            SessionDimensions::default(),
        );

        assert!(result.is_err());
        assert_eq!(manager.inner.claude_hook_registry.registration_count(), 0);
        assert!(!manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .contains_key("failure-session"));
        assert!(!settings_path.exists());
    }

    #[test]
    fn claude_overlay_orphan_sweep_never_removes_a_live_or_unverifiable_owner() {
        let base = temp_test_dir("claude-hook-orphan-sweep");
        let live = base.join("owner-101-1001-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let dead = base.join("owner-202-2002-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let unverifiable = base.join("owner-malformed");
        for root in [&live, &dead, &unverifiable] {
            fs::create_dir_all(root).unwrap();
            fs::write(root.join("copied-settings.json"), b"secret").unwrap();
        }

        let removed = cleanup_orphaned_claude_overlay_roots_at(&base, |pid, started_at| {
            pid == 101 && started_at == 1001
        });

        assert_eq!(removed, 1);
        assert!(live.exists(), "a live DevManager instance owns this root");
        assert!(!dead.exists(), "a verified dead owner is safe to clean");
        assert!(
            unverifiable.exists(),
            "malformed ownership must fail closed rather than risk another instance"
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn explicit_claude_adapter_drain_removes_all_settings_and_the_process_root() {
        let manager = ProcessManager::new();
        let process_root = manager.inner.claude_hook_temp_root.clone();
        let mut launch = AiLaunchSpec {
            tab_id: "drain-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: process_root.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut launch, "drain-session", &process_root);
        let settings_path = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("drain-session")
            .unwrap()
            .settings_path
            .clone();
        assert!(settings_path.exists());

        manager.drain_claude_hook_adapter();

        assert_eq!(manager.inner.claude_hook_registry.registration_count(), 0);
        assert!(manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .is_empty());
        assert!(!settings_path.exists());
        assert!(!process_root.exists());
    }

    #[test]
    fn dropping_the_last_process_manager_handle_drains_claude_overlays() {
        let manager = ProcessManager::new();
        let process_root = manager.inner.claude_hook_temp_root.clone();
        let mut launch = AiLaunchSpec {
            tab_id: "drop-drain-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: process_root.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut launch, "drop-drain-session", &process_root);
        let settings_path = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("drop-drain-session")
            .unwrap()
            .settings_path
            .clone();
        assert!(settings_path.exists());

        drop(manager);

        assert!(!settings_path.exists());
        assert!(!process_root.exists());
    }

    #[test]
    fn dropping_last_process_manager_releases_inner_and_background_workers() {
        let manager = ProcessManager::new();
        let inner = Arc::downgrade(&manager.inner);
        let change_notifier =
            session_change_notifier(manager.inner.clone(), "released-session".to_string());
        let output_notifier =
            session_output_notifier(manager.inner.clone(), "released-session".to_string());

        drop(manager);

        let deadline = Instant::now() + Duration::from_secs(3);
        while inner.upgrade().is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            inner.upgrade().is_none(),
            "the last manager handle must release its inner state and worker ownership"
        );
        change_notifier();
        output_notifier(Vec::new(), TerminalModeSnapshot::default());
    }

    #[test]
    fn dropping_last_process_manager_stops_an_in_progress_background_tick() {
        let manager = ProcessManager::new();
        let inner = Arc::downgrade(&manager.inner);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = Arc::new(Mutex::new(release_rx));
        *manager
            .inner
            .background_test_hook
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(move || {
            let _ = entered_tx.try_send(());
            let _ = release_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv();
        }));

        entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("background worker must reach the controlled checkpoint");

        let stop_inner = inner.clone();
        let (stop_seen_tx, stop_seen_rx) = std::sync::mpsc::sync_channel(1);
        let stop_observer = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            let stop_seen = loop {
                if stop_inner
                    .upgrade()
                    .is_some_and(|inner| inner.background_stop.load(Ordering::SeqCst))
                {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                thread::sleep(Duration::from_millis(10));
            };
            let _ = stop_seen_tx.send(stop_seen);
            let _ = release_tx.send(());
        });

        drop(manager);

        assert!(
            stop_seen_rx.recv().expect("stop observation"),
            "dropping the last manager handle must signal the live background worker"
        );
        stop_observer.join().expect("stop observer");
        assert!(
            inner.upgrade().is_none(),
            "dropping the last manager handle must await background worker release"
        );
    }

    #[test]
    fn internal_process_manager_handle_cannot_defer_native_worker_shutdown() {
        let manager = ProcessManager::new();
        let inner = Arc::downgrade(&manager.inner);
        let internal = process_manager_from_inner(manager.inner.clone())
            .expect("active manager allows an internal handle");

        drop(manager);

        let retained = inner
            .upgrade()
            .expect("internal manager must retain the shared state");
        assert!(
            retained.background_stop.load(Ordering::SeqCst),
            "the last application handle must stop and join native workers even while an internal facade is still borrowed"
        );
        assert!(
            retained
                .background_thread
                .lock()
                .expect("background worker slot")
                .is_none(),
            "shutdown must consume the joined background worker handle"
        );
        assert!(
            retained
                .auto_restart_workers
                .lock()
                .expect("auto-restart worker slots")
                .is_empty(),
            "shutdown must consume every joined auto-restart worker handle"
        );
        drop(retained);

        drop(internal);

        assert!(
            inner.upgrade().is_none(),
            "the non-owning internal facade must release shared state without initiating worker shutdown"
        );
    }

    #[test]
    fn auto_restart_shutdown_waits_for_pre_admission_worker_and_prevents_effect() {
        let manager = ProcessManager::new();
        let launch = configure_auto_restart_race(&manager, "shutdown-auto-restart");

        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = Arc::new(Mutex::new(release_rx));
        *manager
            .inner
            .auto_restart_worker_test_hook
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(move |phase| {
            if phase == AutoRestartWorkerTestPhase::BeforeQueueAdmission {
                let _ = entered_tx.try_send(());
                let _ = release_rx
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .recv();
            }
        }));

        let runtime_state = manager.inner.runtime_state.clone();
        let lifecycle = manager.handle_lifecycle.clone();
        let op_queue = manager.op_queue.clone();
        let inner = Arc::downgrade(&manager.inner);
        handle_auto_restart(&manager.inner);
        entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("auto-restart worker must pause immediately before queue admission");
        assert_eq!(lifecycle_state_for_test(&lifecycle), (1, false));
        assert_eq!(op_queue.successful_submissions_for_test(), 0);

        let (drop_done_tx, drop_done_rx) = std::sync::mpsc::sync_channel(1);
        let dropper = thread::spawn(move || {
            drop(manager);
            let _ = drop_done_tx.send(());
        });
        let shutdown_deadline = Instant::now() + Duration::from_secs(3);
        while lifecycle_state_for_test(&lifecycle) != (0, true)
            && Instant::now() < shutdown_deadline
        {
            thread::yield_now();
        }
        assert_eq!(lifecycle_state_for_test(&lifecycle), (0, true));
        assert!(
            matches!(drop_done_rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)),
            "shutdown must not return while a joined helper is paused at its cancellation checkpoint"
        );
        release_tx.send(()).expect("release auto-restart worker");
        drop_done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("shutdown returns after the helper acknowledges cancellation");
        dropper.join().expect("manager shutdown thread");

        let deadline = Instant::now() + Duration::from_secs(3);
        while inner.upgrade().is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            inner.upgrade().is_none(),
            "rejected auto-restart worker must release the manager inner"
        );
        assert_eq!(lifecycle_state_for_test(&lifecycle), (0, true));
        assert_eq!(op_queue.successful_submissions_for_test(), 0);
        let runtime = runtime_state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let session = runtime
            .sessions
            .get(&launch.command_id)
            .expect("auto-restart runtime state");
        assert_eq!(session.status, SessionStatus::Starting);
        assert_eq!(session.pid, None);
    }

    #[test]
    fn auto_restart_shutdown_fences_worker_that_already_holds_queue_lease() {
        let manager = ProcessManager::new();
        let _launch = configure_auto_restart_race(&manager, "admitted-auto-restart");
        let spawn_hits = Arc::new(AtomicU64::new(0));
        let observed_spawn_hits = spawn_hits.clone();
        *manager
            .inner
            .server_session_spawner_test_hook
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(move |_, _, _| {
            observed_spawn_hits.fetch_add(1, Ordering::SeqCst);
            Err("fixture launch must remain fenced".to_string())
        }));

        let (lease_tx, lease_rx) = std::sync::mpsc::sync_channel(1);
        let (release_lease_tx, release_lease_rx) = std::sync::mpsc::sync_channel(1);
        let release_lease_rx = Arc::new(Mutex::new(release_lease_rx));
        let effect_hits = Arc::new(AtomicU64::new(0));
        let observed_effect_hits = effect_hits.clone();
        *manager
            .inner
            .auto_restart_worker_test_hook
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Arc::new(move |phase| match phase {
                AutoRestartWorkerTestPhase::AfterQueueLease => {
                    let _ = lease_tx.try_send(());
                    let _ = release_lease_rx
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .recv();
                }
                AutoRestartWorkerTestPhase::AfterEffect => {
                    observed_effect_hits.fetch_add(1, Ordering::SeqCst);
                }
                AutoRestartWorkerTestPhase::BeforeQueueAdmission => {}
            }));

        let lifecycle = manager.handle_lifecycle.clone();
        let op_queue = manager.op_queue.clone();
        let inner = Arc::downgrade(&manager.inner);
        handle_auto_restart(&manager.inner);
        lease_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("auto-restart worker must pause after taking a queue lease");
        assert_eq!(lifecycle_state_for_test(&lifecycle), (1, false));
        assert_eq!(op_queue.successful_submissions_for_test(), 0);

        let (drop_done_tx, drop_done_rx) = std::sync::mpsc::sync_channel(1);
        let dropper = thread::spawn(move || {
            drop(manager);
            let _ = drop_done_tx.send(());
        });
        let shutdown_deadline = Instant::now() + Duration::from_secs(3);
        while lifecycle_state_for_test(&lifecycle) != (0, true)
            && Instant::now() < shutdown_deadline
        {
            thread::yield_now();
        }
        assert_eq!(lifecycle_state_for_test(&lifecycle), (0, true));
        assert!(
            matches!(
                drop_done_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "shutdown must join a leased helper before returning"
        );
        release_lease_tx
            .send(())
            .expect("release leased auto-restart worker");
        drop_done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("shutdown returns after leased helper observes queue closure");
        dropper.join().expect("manager shutdown thread");

        let deadline = Instant::now() + Duration::from_secs(3);
        while inner.upgrade().is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(inner.upgrade().is_none());
        assert_eq!(effect_hits.load(Ordering::SeqCst), 0);
        assert_eq!(op_queue.successful_submissions_for_test(), 0);
        assert_eq!(op_queue.completed_operations_for_test(), 0);
        assert_eq!(spawn_hits.load(Ordering::SeqCst), 0);
        assert!(op_queue.drain_completions().is_empty());
    }

    #[test]
    fn process_operation_shutdown_joins_in_flight_effect_and_rejects_late_admission() {
        let manager = ProcessManager::new();
        let launch = configure_auto_restart_race(&manager, "joined-process-operation");
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = Arc::new(Mutex::new(release_rx));
        *manager
            .inner
            .server_session_spawner_test_hook
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(move |_, _, _| {
            let _ = entered_tx.try_send(());
            let _ = release_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv();
            Err("fixture joined operation".to_string())
        }));

        let op_queue = manager.op_queue.clone();
        let lifecycle = manager.handle_lifecycle.clone();
        let inner = Arc::downgrade(&manager.inner);
        op_queue
            .submit(ProcessOp::StartServer {
                op_id: next_op_id(),
                launch: launch.clone(),
                dimensions: SessionDimensions::default(),
                activate: false,
                response: None,
            })
            .expect("admit controlled process operation");
        entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("operation effect must reach controlled checkpoint");

        let (drop_done_tx, drop_done_rx) = std::sync::mpsc::sync_channel(1);
        let dropper = thread::spawn(move || {
            drop(manager);
            let _ = drop_done_tx.send(());
        });
        let shutdown_deadline = Instant::now() + Duration::from_secs(3);
        while lifecycle_state_for_test(&lifecycle) != (0, true)
            && Instant::now() < shutdown_deadline
        {
            thread::yield_now();
        }
        assert_eq!(lifecycle_state_for_test(&lifecycle), (0, true));
        assert!(
            matches!(
                drop_done_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "shutdown must not return while the admitted operation effect is active"
        );
        let late_queue = op_queue.clone();
        let (late_result_tx, late_result_rx) = std::sync::mpsc::sync_channel(1);
        let late_submitter = thread::spawn(move || {
            let result = late_queue.submit(ProcessOp::StartServer {
                op_id: next_op_id(),
                launch,
                dimensions: SessionDimensions::default(),
                activate: false,
                response: None,
            });
            let _ = late_result_tx.send(result);
        });
        assert!(
            matches!(
                late_result_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "late admission must serialize behind the in-progress shutdown boundary"
        );

        release_tx.send(()).expect("release process operation");
        drop_done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("shutdown returns after the operation effect settles and joins");
        dropper.join().expect("manager shutdown thread");
        assert!(
            late_result_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("late admission returns after shutdown linearizes")
                .is_err(),
            "the serialized shutdown fence must reject all later admission"
        );
        late_submitter.join().expect("late admission thread");
        assert_eq!(op_queue.successful_submissions_for_test(), 1);
        assert_eq!(op_queue.completed_operations_for_test(), 1);
        assert!(inner.upgrade().is_none());
    }

    #[test]
    fn process_manager_registers_one_shared_operation_queue() {
        let manager = ProcessManager::new();
        let inner_queue = manager
            .inner
            .op_queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .upgrade()
            .expect("inner operation queue");

        assert!(Arc::ptr_eq(&manager.op_queue, &inner_queue));
    }

    #[test]
    fn task_terminal_authority_preserves_owner_resource_and_monotonic_epochs() {
        let manager = ProcessManager::new();
        let task_id = TaskId::new();
        let first = manager
            .issue_task_terminal_launch_authority(task_id, "task-terminal", &[8080, 8080])
            .expect("first Task terminal authority");
        let second = manager
            .issue_task_terminal_launch_authority(task_id, "task-terminal", &[8080])
            .expect("replacement Task terminal authority");

        let (first_owner, first_resource, first_generation, first_epoch) =
            first.identity_for_test();
        let (second_owner, second_resource, second_generation, second_epoch) =
            second.identity_for_test();
        assert_eq!(first_owner, ProcessOwner::Task(task_id));
        assert_eq!(second_owner, ProcessOwner::Task(task_id));
        assert_eq!(first_resource, second_resource);
        assert_eq!(first_generation, 1);
        assert_eq!(second_generation, 2);
        assert!(second_epoch > first_epoch);

        let oversized_ports = vec![0; MAX_MANAGED_TERMINAL_PORTS + 1];
        assert!(manager
            .issue_task_terminal_launch_authority(task_id, "task-terminal", &oversized_ports,)
            .is_err());
    }

    #[test]
    fn clear_virtual_output_resets_terminal_snapshot() {
        let cwd = temp_test_dir("clear-virtual-output");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "test-shell";

        manager
            .spawn_shell_session(session_id, &cwd, SessionDimensions::default(), None, None)
            .unwrap();
        manager
            .write_virtual_text(session_id, "hello world\r\n")
            .unwrap();

        let before = manager.session_view(session_id).expect("session view");
        assert!(screen_text(&before).contains("hello world"));

        manager.clear_virtual_output(session_id).unwrap();
        let after = manager.session_view(session_id).expect("session view");
        assert!(!screen_text(&after).contains("hello world"));

        let _ = manager.close_session(session_id);
    }

    #[test]
    fn restart_server_preserves_or_clears_logs_based_on_setting() {
        for clear_logs_on_restart in [false, true] {
            let cwd = temp_test_dir(if clear_logs_on_restart {
                "restart-clear-logs"
            } else {
                "restart-preserve-logs"
            });
            let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
            let manager = ProcessManager::new();
            let mut app_state = app_state_with_server(&cwd, clear_logs_on_restart);
            let command_id = "server-cmd";
            let dimensions = SessionDimensions::default();

            manager
                .start_server(&mut app_state, command_id, dimensions)
                .unwrap();
            wait_for_live_session(&manager, command_id);
            manager
                .write_virtual_text(command_id, "stale output\r\n")
                .unwrap();

            manager
                .restart_server(&mut app_state, command_id, dimensions)
                .unwrap();
            for _ in 0..50 {
                let _ = manager.drain_process_op_completions();
                if manager
                    .session_view(command_id)
                    .map(|view| screen_text(&view).contains("Restarting"))
                    .unwrap_or(false)
                {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            wait_for_live_session(&manager, command_id);

            let view = manager
                .session_view(command_id)
                .expect("server session view");
            let text = screen_text(&view);
            assert!(text.contains("Restarting"));
            if clear_logs_on_restart {
                assert!(!text.contains("stale output"));
            } else {
                assert!(text.contains("stale output"));
            }

            let _ = manager.stop_server(command_id);
        }
    }

    #[test]
    fn shutdown_managed_processes_prunes_tracked_processes() {
        let cwd = temp_test_dir("managed-shutdown");
        let pid_file_path = cwd.join("running-pids.json");
        let _pid_file_guard = pid_file::use_test_pid_file(pid_file_path);
        let manager = ProcessManager::new();
        let mut app_state = app_state_with_server(&cwd, true);
        let command_id = "server-cmd";
        let dimensions = SessionDimensions::default();

        manager
            .start_server(&mut app_state, command_id, dimensions)
            .unwrap();
        wait_for_live_session(&manager, command_id);
        wait_for_tracked_process(command_id);
        assert!(!pid_file::tracked_pids().is_empty());

        let report = manager.shutdown_managed_processes(Duration::from_secs(5));

        assert_eq!(report.requested_sessions, 1);
        assert_eq!(report.remaining_live_sessions, 0);
        assert_eq!(report.remaining_tracked_pids, 0);
        wait_for_tracked_processes_to_clear();
    }

    #[test]
    fn shell_sessions_are_tracked_in_managed_pid_ledger() {
        let cwd = temp_test_dir("managed-shell");
        let pid_file_path = cwd.join("running-pids.json");
        let _pid_file_guard = pid_file::use_test_pid_file(pid_file_path);
        let manager = ProcessManager::new();
        let session_id = "shell-session";

        manager
            .spawn_shell_session(session_id, &cwd, SessionDimensions::default(), None, None)
            .unwrap();
        wait_for_live_session(&manager, session_id);
        wait_for_tracked_process(session_id);

        let tracked = pid_file::tracked_processes();
        let shell_entry = tracked
            .iter()
            .find(|entry| entry.session_id == session_id)
            .expect("shell session was not tracked");
        assert_eq!(shell_entry.session_kind, "shell");
        assert!(pid_file::tracked_pids().contains(&shell_entry.pid));

        let _ = manager.close_session(session_id);
    }

    #[test]
    fn stopped_server_can_start_again_with_fresh_terminal_authority() {
        let cwd = temp_test_dir("restart-after-stop");
        let pid_file_path = cwd.join("running-pids.json");
        let _pid_file_guard = pid_file::use_test_pid_file(pid_file_path);
        let manager = ProcessManager::new();
        let mut app_state = app_state_with_server(&cwd, true);
        let command_id = "server-cmd";
        let dimensions = SessionDimensions::default();

        manager
            .start_server(&mut app_state, command_id, dimensions)
            .unwrap();
        wait_for_running_session(&manager, command_id);
        #[cfg(windows)]
        let first_generation = manager
            .get_session(command_id)
            .expect("first terminal owner")
            .managed_process_snapshot()
            .expect("first exact managed snapshot")
            .0
            .resource()
            .runtime_generation;

        assert!(manager.stop_server_and_wait(command_id, Duration::from_secs(5)));
        wait_for_stopped_session(&manager, command_id);

        manager
            .start_server(&mut app_state, command_id, dimensions)
            .unwrap();
        wait_for_running_session(&manager, command_id);
        #[cfg(windows)]
        assert!(
            manager
                .get_session(command_id)
                .expect("replacement terminal owner")
                .managed_process_snapshot()
                .expect("replacement exact managed snapshot")
                .0
                .resource()
                .runtime_generation
                > first_generation,
            "a stopped terminal must never reuse its released process authority"
        );
    }

    #[test]
    fn set_active_session_does_not_create_placeholder_runtime_entry() {
        let manager = ProcessManager::new();

        manager.set_active_session("missing-session");

        let runtime = manager.runtime_state();
        assert_eq!(
            runtime.active_session_id.as_deref(),
            Some("missing-session")
        );
        assert!(!runtime.sessions.contains_key("missing-session"));
    }

    #[test]
    fn ai_session_does_not_need_restore_during_fresh_unattached_startup_gap() {
        let now = Instant::now();
        let mut session = SessionRuntimeState::new(
            "claude-session",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.session_kind = SessionKind::Claude;
        session.status = SessionStatus::Starting;

        assert!(!ai_session_needs_restore(Some(&session), false, now));

        session.status = SessionStatus::Running;
        session.started_at = Some(now);
        assert!(!ai_session_needs_restore(Some(&session), false, now));

        session.started_at = Some(now - Duration::from_secs(31));
        assert!(ai_session_needs_restore(Some(&session), false, now));

        assert!(!ai_session_needs_restore(Some(&session), true, now));
        assert!(ai_session_needs_restore(None, false, now));
    }

    #[test]
    fn detects_blocking_external_editor_children() {
        assert!(is_blocking_external_editor_name("Code.exe"));
        assert!(is_blocking_external_editor_name("cursor"));
        assert!(!is_blocking_external_editor_name("node.exe"));
    }

    #[test]
    fn absent_runtime_projection_with_live_ledger_evidence_remains_retryable() {
        let cwd = temp_test_dir("authority-dead-root-descendant");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let current = platform_service::capture_process_identity(std::process::id())
            .expect("current process identity");
        pid_file::track_session_process(pid_file::ManagedProcessRecord {
            session_id: "server-cmd".to_string(),
            pid: u32::MAX,
            started_at_unix_secs: 1,
            process_name: Some("missing-root.exe".to_string()),
            session_kind: "server".to_string(),
            program: "cmd".to_string(),
            project_id: Some("project-1".to_string()),
            command_id: Some("server-cmd".to_string()),
            tab_id: None,
            descendant_processes: vec![pid_file::TrackedProcessIdentity {
                pid: current.pid,
                started_at_unix_secs: current.started_at_unix_secs,
                process_name: current.process_name,
            }],
        })
        .unwrap();

        let error = retry_exact_session_teardown(&manager.inner, "server-cmd")
            .expect_err("live exact ledger evidence must prevent a forged stopped result");
        assert!(error.contains("authority"), "{error}");
        assert!(platform_service::is_pid_running(std::process::id()));
        assert_eq!(
            pid_file::active_tracked_pids_for_session("server-cmd"),
            vec![std::process::id()],
            "crash-recovery evidence remains retained without a live Job authority"
        );
    }

    #[test]
    fn replacement_is_rejected_while_unowned_live_ledger_identity_remains() {
        let cwd = temp_test_dir("replacement-live-ledger");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let current = platform_service::capture_process_identity(std::process::id())
            .expect("current process identity");
        pid_file::track_session_process(pid_file::ManagedProcessRecord {
            session_id: "stale-session".to_string(),
            pid: current.pid,
            started_at_unix_secs: current.started_at_unix_secs,
            process_name: current.process_name,
            session_kind: "server".to_string(),
            program: "cmd".to_string(),
            project_id: Some("project-1".to_string()),
            command_id: Some("stale-session".to_string()),
            tab_id: None,
            descendant_processes: Vec::new(),
        })
        .expect("track live ledger identity");

        let error =
            ensure_prior_session_teardown_settled(&manager.inner, "stale-session", Duration::ZERO)
                .expect_err("replacement must fail closed without a live Job authority");
        assert!(error.contains("did not settle before replacement"));
        assert!(platform_service::is_pid_running(std::process::id()));
    }

    #[test]
    fn replacement_admission_accepts_absent_runtime_without_ledger() {
        let cwd = temp_test_dir("replacement-absent-runtime");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();

        ensure_prior_session_teardown_settled(&manager.inner, "absent-runtime", Duration::ZERO)
            .expect("absence from both authoritative process sources is replaceable");

        assert!(
            !manager
                .runtime_state()
                .sessions
                .contains_key("absent-runtime"),
            "replacement admission must not synthesize a runtime row"
        );
    }

    #[test]
    fn replacement_admission_scrubs_diagnostic_only_starting_projection() {
        let cwd = temp_test_dir("replacement-starting-diagnostics");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let mut session = SessionRuntimeState::new(
            "starting-diagnostics",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.resources.metrics_unavailable = true;
        session.resources.metrics_status = ProcessMetricStatus::Failed;
        session.resources.metric_values = ResourceMetricValueState::LastKnown;
        session.resources.cpu_value_state = ResourceMetricValueState::LastKnown;
        session.resources.memory_value_state = ResourceMetricValueState::LastKnown;
        session.resources.process_count_value_state = ResourceMetricValueState::LastKnown;
        session.resources.metrics_stale = true;
        session.resources.metrics_error = Some("prior_sample".to_string());
        session.resources.sampling_generation = 41;
        session.resources.io_read_bytes = Some(42);
        session.resources.io_write_bytes = Some(43);
        session.resources.logical_cpu_count = 16;
        session.resources.last_sample_at = Some(Instant::now());
        session.exit = Some(SessionExitState {
            code: Some(17),
            signal: Some("starting-signal".to_string()),
            closed_by_user: false,
            summary: "starting-exit-metadata".to_string(),
        });
        manager.register_runtime_session(session);

        ensure_prior_session_teardown_settled(
            &manager.inner,
            "starting-diagnostics",
            Duration::ZERO,
        )
        .expect("an ownerless prelaunch row without ledger evidence is replaceable");

        let runtime = manager.runtime_state();
        let session = runtime
            .sessions
            .get("starting-diagnostics")
            .expect("runtime row");
        assert_eq!(session.status, SessionStatus::Starting);
        let exit = session.exit.as_ref().expect("seeded exit metadata");
        assert_eq!(exit.code, Some(17));
        assert_eq!(exit.signal.as_deref(), Some("starting-signal"));
        assert!(!exit.closed_by_user);
        assert_eq!(exit.summary, "starting-exit-metadata");
        assert!(!session.resources.metrics_unavailable);
        assert_eq!(
            session.resources.metrics_status,
            ProcessMetricStatus::Unknown
        );
        assert_eq!(
            session.resources.metric_values,
            ResourceMetricValueState::Unavailable
        );
        assert_eq!(
            session.resources.cpu_value_state,
            ResourceMetricValueState::Unavailable
        );
        assert_eq!(
            session.resources.memory_value_state,
            ResourceMetricValueState::Unavailable
        );
        assert_eq!(
            session.resources.process_count_value_state,
            ResourceMetricValueState::Unavailable
        );
        assert!(!session.resources.metrics_stale);
        assert!(session.resources.metrics_error.is_none());
        assert_eq!(session.resources.sampling_generation, 0);
        assert!(session.resources.io_read_bytes.is_none());
        assert!(session.resources.io_write_bytes.is_none());
        assert_eq!(session.resources.logical_cpu_count, 1);
        assert!(session.resources.last_sample_at.is_none());
    }

    #[test]
    fn replacement_rejects_ownerless_reap_incomplete_without_live_ledger() {
        let cwd = temp_test_dir("replacement-reap-incomplete");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let mut session = SessionRuntimeState::new(
            "replacement-reap-incomplete",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.status = SessionStatus::Failed;
        session.reap_incomplete = true;
        manager.register_runtime_session(session);

        let error = ensure_prior_session_teardown_settled(
            &manager.inner,
            "replacement-reap-incomplete",
            Duration::ZERO,
        )
        .expect_err("reap-incomplete state remains fail-closed without exact release");
        assert!(
            error.contains("did not settle before replacement"),
            "{error}"
        );

        let runtime = manager.runtime_state();
        let session = runtime
            .sessions
            .get("replacement-reap-incomplete")
            .expect("runtime residue");
        assert_eq!(session.status, SessionStatus::Failed);
        assert!(session.reap_incomplete);
        assert_process_monitor_has_no_kill_authority(session);
        assert_eq!(
            session.resources.metrics_error.as_deref(),
            Some("exact_owner_unavailable"),
            "the explicit retry path records why exact teardown is unavailable"
        );
    }

    #[test]
    fn replacement_safety_does_not_publish_stopped_without_exact_release() {
        let cwd = temp_test_dir("settled-stopped-session");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let stale_pid = 800_002;
        let mut session = SessionRuntimeState::new(
            "alpha",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.status = SessionStatus::Stopping;
        session.pid = Some(stale_pid);
        session.resources =
            ownerless_process_projection("alpha", stale_pid, synthetic_process_fence(stale_pid));
        session.exit = Some(SessionExitState {
            code: Some(23),
            signal: Some("stopping-signal".to_string()),
            closed_by_user: true,
            summary: "stopping-exit-metadata".to_string(),
        });
        manager.register_runtime_session(session);

        assert!(manager.ensure_session_replacement_safe_for_test("alpha", Duration::from_millis(1)));

        let runtime = manager.runtime_state();
        let session = runtime.sessions.get("alpha").expect("runtime row");
        assert_eq!(
            session.status,
            SessionStatus::Stopping,
            "replacement safety is not proof of exact Job release and must not publish Stopped"
        );
        let exit = session.exit.as_ref().expect("seeded exit metadata");
        assert_eq!(exit.code, Some(23));
        assert_eq!(exit.signal.as_deref(), Some("stopping-signal"));
        assert!(exit.closed_by_user);
        assert_eq!(exit.summary, "stopping-exit-metadata");
        assert_process_monitor_has_no_kill_authority(session);
    }

    #[test]
    fn close_tab_removes_ssh_tab_and_stops_session() {
        let cwd = temp_test_dir("close-ssh-tab");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let mut app_state = AppState::default();
        app_state.open_tabs.push(SessionTab {
            id: "ssh-tab".to_string(),
            tab_type: TabType::Ssh,
            project_id: "project-1".to_string(),
            command_id: None,
            pty_session_id: Some("ssh-session".to_string()),
            provider_session_id: None,
            label: Some("SSH".to_string()),
            ssh_connection_id: Some("ssh-1".to_string()),
            browser_workspace: None,
        });
        manager.register_runtime_session(SessionRuntimeState::new(
            "ssh-session",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        ));

        manager.close_tab(&mut app_state, "ssh-tab").unwrap();
        for _ in 0..50 {
            let _ = manager.drain_process_op_completions();
            let status = manager
                .runtime_state()
                .sessions
                .get("ssh-session")
                .map(|session| session.status);
            if matches!(
                status,
                Some(SessionStatus::Stopped) | Some(SessionStatus::Failed) | None
            ) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }

        let runtime = manager.runtime_state();
        assert!(app_state.find_tab("ssh-tab").is_none());
        let status = runtime
            .sessions
            .get("ssh-session")
            .map(|session| session.status);
        assert!(
            matches!(
                status,
                Some(SessionStatus::Stopped) | Some(SessionStatus::Failed) | None
            ),
            "expected ssh session to stop or be removed, got {status:?}"
        );
    }

    #[test]
    fn schedule_start_server_returns_immediately() {
        let cwd = temp_test_dir("schedule-start-immediate");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let mut app_state = app_state_with_server(&cwd, true);
        let started = Instant::now();
        manager
            .start_server_in_background(&mut app_state, "server-cmd", SessionDimensions::default())
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "start_server_in_background blocked for {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn sanitize_private_key_normalizes_line_endings_and_trailing_newline() {
        let pasted =
            "-----BEGIN OPENSSH PRIVATE KEY-----\r\nabc\r\n-----END OPENSSH PRIVATE KEY-----";
        assert_eq!(
            sanitize_private_key(pasted),
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n"
        );
    }

    #[test]
    fn sanitize_private_key_leaves_clean_key_unchanged() {
        let clean = "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n";
        assert_eq!(sanitize_private_key(clean), clean);
    }

    #[test]
    fn sanitize_private_key_trims_surrounding_blank_lines() {
        let pasted = "\n\n  -----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n\n\n";
        assert_eq!(
            sanitize_private_key(pasted),
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n"
        );
    }

    fn ssh_test_connection() -> SSHConnection {
        SSHConnection {
            id: "ssh-1".to_string(),
            label: "Prod".to_string(),
            host: "example.com".to_string(),
            port: 2222,
            username: "deploy".to_string(),
            password: None,
            private_key: None,
        }
    }

    fn ssh_test_tab() -> SessionTab {
        SessionTab {
            id: "ssh-tab-1".to_string(),
            tab_type: TabType::Ssh,
            project_id: "project-1".to_string(),
            ssh_connection_id: Some("ssh-1".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn build_ssh_launch_spec_without_key_omits_identity_flag() {
        let state = AppState::default();

        let launch = build_ssh_launch_spec(&state, &ssh_test_tab(), &ssh_test_connection(), None);

        assert_eq!(launch.program, "ssh");
        assert_eq!(
            launch.args,
            vec![
                "deploy@example.com".to_string(),
                "-p".to_string(),
                "2222".to_string(),
            ]
        );
    }

    #[test]
    fn build_ssh_launch_spec_with_key_appends_identity_flag() {
        let state = AppState::default();
        let key_file = PathBuf::from("/keys/ssh-1");

        let launch = build_ssh_launch_spec(
            &state,
            &ssh_test_tab(),
            &ssh_test_connection(),
            Some(key_file.as_path()),
        );

        assert_eq!(
            launch.args,
            vec![
                "deploy@example.com".to_string(),
                "-p".to_string(),
                "2222".to_string(),
                "-i".to_string(),
                key_file.display().to_string(),
            ]
        );
    }

    #[test]
    fn safe_key_file_name_replaces_path_hostile_characters() {
        assert_eq!(safe_key_file_name("ssh-1a2b-3"), "ssh-1a2b-3");
        assert_eq!(safe_key_file_name("ssh/../evil"), "ssh____evil");
    }

    #[test]
    fn materialize_ssh_key_writes_sanitized_key_file() {
        let dir = temp_test_dir("materialize-ssh-key");
        let connection = SSHConnection {
            id: "ssh-test".to_string(),
            label: "Test".to_string(),
            host: "example.com".to_string(),
            port: 22,
            username: "deploy".to_string(),
            password: None,
            private_key: Some("-----BEGIN KEY-----\r\nabc\r\n-----END KEY-----".to_string()),
        };

        let path = materialize_ssh_key_in(&dir, &connection)
            .expect("materialize")
            .expect("path");

        assert_eq!(path, dir.join("ssh-test"));
        assert_eq!(
            fs::read_to_string(&path).expect("read key"),
            "-----BEGIN KEY-----\nabc\n-----END KEY-----\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dir_mode = fs::metadata(&dir)
                .expect("dir metadata")
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }
    }

    #[test]
    fn materialize_ssh_key_rejects_empty_connection_id() {
        let dir = temp_test_dir("materialize-ssh-key-empty-id");
        let connection = SSHConnection {
            id: String::new(),
            label: "Test".to_string(),
            host: "example.com".to_string(),
            port: 22,
            username: "deploy".to_string(),
            password: None,
            private_key: Some("-----BEGIN KEY-----\nabc\n-----END KEY-----".to_string()),
        };

        let error = materialize_ssh_key_in(&dir, &connection).expect_err("should reject");
        assert!(error.contains("connection id"), "unexpected error: {error}");
    }

    #[test]
    fn sanitize_private_key_normalizes_lone_carriage_returns() {
        let input = "-----BEGIN KEY-----\rabc\r-----END KEY-----";
        assert_eq!(
            sanitize_private_key(input),
            "-----BEGIN KEY-----\nabc\n-----END KEY-----\n"
        );
    }

    #[test]
    fn materialize_ssh_key_returns_none_without_key_material() {
        let dir = temp_test_dir("materialize-ssh-key-empty");
        let connection = SSHConnection {
            id: "ssh-empty".to_string(),
            label: "Test".to_string(),
            host: "example.com".to_string(),
            port: 22,
            username: "deploy".to_string(),
            password: Some("pw".to_string()),
            private_key: Some("   \n".to_string()),
        };

        assert_eq!(materialize_ssh_key_in(&dir, &connection), Ok(None));
        assert!(!dir.join("ssh-empty").exists());
    }

    fn temp_test_dir(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("devmanager-tests-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    mod sealed_fence_issuer {
        use super::*;

        trait Sealed {}

        struct Issuer;

        impl Sealed for Issuer {}

        trait IssueExactFence: Sealed {
            fn issue(
                &self,
                seed: u8,
                generation: u64,
                owner: ProcessOwner,
                pid: u32,
                creation_time_100ns: u64,
            ) -> ManagedProcessFence;
        }

        impl IssueExactFence for Issuer {
            fn issue(
                &self,
                seed: u8,
                generation: u64,
                owner: ProcessOwner,
                pid: u32,
                creation_time_100ns: u64,
            ) -> ManagedProcessFence {
                let mut resource_bytes = [
                    0x01, 0x9a, 0x11, 0x22, 0x33, 0x44, 0x70, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00,
                ];
                resource_bytes[15] = seed;
                let resource_id = ResourceId::from_bytes(resource_bytes).expect("resource id");
                let identity = ManagedProcessIdentity::new(
                    crate::process::identity::ManagedProcessId::new(pid, creation_time_100ns)
                        .expect("non-zero test process identity"),
                    std::env::current_exe().expect("test executable"),
                )
                .expect("canonical test executable");
                ManagedProcessFence::new(
                    ResourceFence::new(resource_id, generation),
                    owner,
                    identity,
                )
            }
        }

        pub(super) fn issue(
            seed: u8,
            generation: u64,
            owner: ProcessOwner,
            pid: u32,
            creation_time_100ns: u64,
        ) -> ManagedProcessFence {
            Issuer.issue(seed, generation, owner, pid, creation_time_100ns)
        }
    }

    fn synthetic_process_fence(pid: u32) -> ManagedProcessFence {
        sealed_fence_issuer::issue(1, 1, ProcessOwner::Host, pid, 1)
    }

    fn app_state_with_server(cwd: &Path, clear_logs_on_restart: bool) -> AppState {
        let (command_text, args) = server_test_command();
        let command = RunCommand {
            id: "server-cmd".to_string(),
            label: "Server".to_string(),
            command: command_text,
            args,
            env: None,
            // These lifecycle fixtures exercise process/session ownership,
            // not port settlement.  The test command is `ping`/`sleep` and
            // deliberately does not bind a listener; declaring a port would
            // make the strict post-launch admission path reap it as an
            // unsettled launch before the lifecycle assertions run.
            port: None,
            auto_restart: Some(false),
            clear_logs_on_restart: Some(clear_logs_on_restart),
        };
        let folder = ProjectFolder {
            id: "folder-1".to_string(),
            name: "Folder".to_string(),
            folder_path: cwd.to_string_lossy().to_string(),
            commands: vec![command],
            env_file_path: None,
            port_variable: None,
            hidden: Some(false),
        };
        let project = Project {
            id: "project-1".to_string(),
            name: "Project".to_string(),
            root_path: cwd.to_string_lossy().to_string(),
            folders: vec![folder],
            color: None,
            pinned: Some(false),
            notes: None,
            save_log_files: Some(false),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };

        let mut state = AppState::default();
        state.config = AppConfig {
            version: crate::models::CURRENT_CONFIG_VERSION,
            projects: vec![project],
            settings: Settings::default(),
            ssh_connections: Vec::new(),
        };
        state.mark_dirty();
        state
    }

    #[test]
    fn runtime_revision_tracks_semantic_changes_but_not_frame_metrics() {
        let manager = ProcessManager::new();
        let initial_revision = manager.runtime_revision();
        manager.register_runtime_session(SessionRuntimeState::new(
            "alpha",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        ));
        let after_register = manager.runtime_revision();
        assert!(after_register > initial_revision);

        let runtime_events = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let event_counter = runtime_events.clone();
        manager.set_remote_session_handler(Some(std::sync::Arc::new(move |event| {
            if matches!(event, RemoteSessionEvent::Runtime { .. }) {
                event_counter.fetch_add(1, Ordering::SeqCst);
            }
        })));
        runtime_events.store(0, Ordering::SeqCst);

        manager.record_frame("alpha", Duration::from_millis(4));
        assert_eq!(runtime_events.load(Ordering::SeqCst), 0);
        assert_eq!(manager.runtime_revision(), after_register);

        manager.set_active_session("alpha");
        let after_active = manager.runtime_revision();
        assert!(after_active > after_register);

        manager.set_active_session("alpha");
        assert_eq!(manager.runtime_revision(), after_active);
    }

    #[test]
    fn server_lifecycle_generation_advances_for_queued_server_operations() {
        let manager = ProcessManager::new();
        let before = manager.server_lifecycle_generation();

        manager
            .submit_process_op(ProcessOp::StopAll {
                op_id: next_op_id(),
                command_ids: Vec::new(),
                wait: Duration::ZERO,
                response: None,
            })
            .expect("queue StopAll");

        assert!(manager.server_lifecycle_generation() > before);
        stop_background_tasks_for_test(&manager);
    }

    #[test]
    fn shutdown_queue_admission_invalidates_server_lifecycle_before_worker_runs() {
        let manager = ProcessManager::new();
        let before = manager.server_lifecycle_generation();

        manager
            .submit_process_op(ProcessOp::Shutdown {
                op_id: next_op_id(),
                timeout: Duration::ZERO,
            })
            .expect("queue Shutdown");

        assert!(manager.server_lifecycle_generation() > before);
        stop_background_tasks_for_test(&manager);
    }

    #[test]
    fn direct_process_manager_drop_bumps_server_lifecycle_before_close() {
        let manager = ProcessManager::new();
        let inner = manager.inner.clone();
        let before = manager.server_lifecycle_generation();

        drop(manager);

        assert!(
            inner.server_lifecycle_generation.load(Ordering::Acquire) > before,
            "direct ProcessManager drop must fence late lifecycle callbacks before close"
        );
    }

    #[test]
    fn session_change_notifier_only_emits_when_dirty_generation_advances() {
        let manager = ProcessManager::new();
        manager.register_runtime_session(SessionRuntimeState::new(
            "alpha",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        ));
        let runtime_events = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let event_counter = runtime_events.clone();
        manager.set_remote_session_handler(Some(std::sync::Arc::new(move |event| {
            if matches!(event, RemoteSessionEvent::Runtime { .. }) {
                event_counter.fetch_add(1, Ordering::SeqCst);
            }
        })));
        runtime_events.store(0, Ordering::SeqCst);

        let notifier = session_change_notifier(manager.inner.clone(), "alpha".to_string());
        let initial_revision = manager.runtime_revision();
        notifier();
        assert_eq!(runtime_events.load(Ordering::SeqCst), 0);
        assert_eq!(manager.runtime_revision(), initial_revision);

        if let Ok(mut runtime) = manager.inner.runtime_state.write() {
            if let Some(session) = runtime.sessions.get_mut("alpha") {
                session.note_title(Some("ready".to_string()));
            }
        }

        notifier();
        let after_change = manager.runtime_revision();
        assert_eq!(runtime_events.load(Ordering::SeqCst), 1);
        assert!(after_change > initial_revision);

        notifier();
        assert_eq!(runtime_events.load(Ordering::SeqCst), 1);
        assert_eq!(manager.runtime_revision(), after_change);
    }

    #[test]
    fn record_frame_does_not_block_on_busy_runtime_lock() {
        let manager = ProcessManager::new();
        manager.register_runtime_session(SessionRuntimeState::new(
            "alpha",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        ));

        let runtime_guard = manager
            .inner
            .runtime_state
            .read()
            .expect("runtime read lock");
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = manager.clone();
        let handle = thread::spawn(move || {
            worker.record_frame("alpha", Duration::from_millis(1));
            tx.send(()).expect("record_frame completion");
        });

        let completed_while_locked = rx.recv_timeout(Duration::from_millis(50));
        drop(runtime_guard);
        handle.join().expect("record_frame thread joined");

        assert!(
            completed_while_locked.is_ok(),
            "record_frame blocked on runtime lock"
        );
    }

    #[cfg(windows)]
    #[test]
    fn exact_process_action_rejects_a_foreign_fence() {
        let cwd = temp_test_dir("kill-reject-foreign");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "shell-kill-reject";

        manager
            .spawn_shell_session(session_id, &cwd, SessionDimensions::default(), None, None)
            .unwrap();
        wait_for_live_session(&manager, session_id);

        let fence = synthetic_process_fence(
            manager
                .runtime_state()
                .sessions
                .get(session_id)
                .and_then(|session| session.pid)
                .expect("live pid"),
        );
        let foreign_pid = 4_294_967_294;
        let completion = execute_process_op_inner(
            &manager.inner,
            ProcessOp::KillProcess {
                op_id: next_op_id(),
                session_id: session_id.to_string(),
                pid: foreign_pid,
                fence,
                response: None,
            },
        );
        assert!(completion.result.is_err());
        assert!(completion
            .result
            .unwrap_err()
            .contains("generation changed"));

        let _ = manager.close_session(session_id);
    }

    fn ownerless_process_projection(
        session_id: &str,
        pid: u32,
        fence: ManagedProcessFence,
    ) -> ResourceSnapshot {
        ResourceSnapshot {
            process_count: 1,
            process_count_value_state: ResourceMetricValueState::LastKnown,
            process_ids: vec![pid],
            processes: vec![crate::state::ProcessResourceNode {
                pid,
                parent_pid: None,
                name: "ownerless".to_string(),
                cpu_percent: 0.0,
                core_equivalent_percent: 0.0,
                memory_bytes: 0,
                memory_metric: resource_memory_metric(),
                creation_time_100ns: None,
                executable: None,
                command_label: None,
                command_arg_count: 0,
                command_arg_bytes: 0,
                resource_id: Some(opaque_resource_id(session_id)),
                resource_kind: None,
                child_count: 0,
                lifecycle: crate::state::ProcessResourceLifecycle::Failed,
                metrics_status: crate::domain::snapshot::ProcessMetricStatus::Unknown,
                metric_values: ResourceMetricValueState::Unavailable,
                cpu_value_state: ResourceMetricValueState::Unavailable,
                memory_value_state: ResourceMetricValueState::Unavailable,
                sampling_generation: 0,
            }],
            managed_process_fence: Some(fence),
            ..ResourceSnapshot::default()
        }
    }

    fn assert_process_monitor_has_no_kill_authority(session: &SessionRuntimeState) {
        // The process monitor only constructs Kill/Kill-tree actions when the
        // projected row carries an exact managed-process fence.
        assert!(session.pid.is_none());
        assert_eq!(session.resources.process_count, 0);
        assert!(session.resources.process_ids.is_empty());
        assert!(session.resources.processes.is_empty());
        assert!(session.resources.managed_process_fence.is_none());
    }

    #[test]
    fn request_close_cleans_ownerless_stopped_projection_before_settlement() {
        let cwd = temp_test_dir("ownerless-stopped-close");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "ownerless-stopped-close";
        let stale_pid = 800_001;
        let mut session = SessionRuntimeState::new(
            session_id,
            cwd,
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.status = SessionStatus::Stopped;
        session.pid = Some(stale_pid);
        session.resources =
            ownerless_process_projection(session_id, stale_pid, synthetic_process_fence(stale_pid));
        manager.register_runtime_session(session);

        manager
            .request_session_close(session_id, true)
            .expect("a clean ownerless Stopped row may settle only after projection cleanup");

        let runtime = manager.runtime_state();
        let session = runtime.sessions.get(session_id).expect("runtime row");
        assert_eq!(session.status, SessionStatus::Stopped);
        assert!(!session.reap_incomplete);
        assert_process_monitor_has_no_kill_authority(session);
        assert!(session_projection_is_already_settled(
            &manager.inner,
            session_id
        ));
    }

    #[test]
    fn request_close_cleans_ownerless_failed_projection_but_retains_live_ledger_evidence() {
        let cwd = temp_test_dir("ownerless-failed-close");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "ownerless-failed-close";
        let current = platform_service::capture_process_identity(std::process::id())
            .expect("current process identity");
        pid_file::track_session_process(pid_file::ManagedProcessRecord {
            session_id: session_id.to_string(),
            pid: current.pid,
            started_at_unix_secs: current.started_at_unix_secs,
            process_name: current.process_name.clone(),
            session_kind: "shell".to_string(),
            program: "test-shell".to_string(),
            project_id: None,
            command_id: None,
            tab_id: None,
            descendant_processes: Vec::new(),
        })
        .expect("track live recovery evidence");
        let mut session = SessionRuntimeState::new(
            session_id,
            cwd,
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.status = SessionStatus::Failed;
        session.reap_incomplete = true;
        session.pid = Some(current.pid);
        session.resources = ownerless_process_projection(
            session_id,
            current.pid,
            synthetic_process_fence(current.pid),
        );
        manager.register_runtime_session(session);

        let error = manager
            .request_session_close(session_id, true)
            .expect_err("live ledger evidence keeps missing-owner teardown retryable");
        assert!(error.contains("Unknown session"), "{error}");

        let runtime = manager.runtime_state();
        let session = runtime.sessions.get(session_id).expect("runtime residue");
        assert_eq!(session.status, SessionStatus::Failed);
        assert!(session.reap_incomplete);
        assert_process_monitor_has_no_kill_authority(session);
        assert_eq!(
            pid_file::active_tracked_pids_for_session(session_id),
            vec![current.pid],
            "exact live ledger evidence remains for reconciliation"
        );
        assert!(platform_service::is_pid_running(current.pid));
        assert!(!session_projection_is_already_settled(
            &manager.inner,
            session_id
        ));
    }

    #[test]
    fn exact_process_action_rejects_a_stale_resource_row_without_owner() {
        let cwd = temp_test_dir("kill-reject-stale");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "stale-kill-session";
        let running_pid = std::process::id();
        let stale_fence = synthetic_process_fence(running_pid);

        {
            let mut runtime = manager.inner.runtime_state.write().expect("runtime write");
            let mut session = SessionRuntimeState::new(
                session_id,
                cwd.clone(),
                SessionDimensions::default(),
                TerminalBackend::PortablePtyFeedingAlacritty,
            );
            session.status = SessionStatus::Failed;
            session.reap_incomplete = true;
            session.pid = Some(running_pid);
            session.resources = ResourceSnapshot {
                process_count: 1,
                process_count_value_state: ResourceMetricValueState::LastKnown,
                process_ids: vec![running_pid],
                processes: vec![crate::state::ProcessResourceNode {
                    pid: running_pid,
                    parent_pid: None,
                    name: "stale".to_string(),
                    cpu_percent: 0.0,
                    core_equivalent_percent: 0.0,
                    memory_bytes: 0,
                    memory_metric: resource_memory_metric(),
                    creation_time_100ns: None,
                    executable: None,
                    command_label: None,
                    command_arg_count: 0,
                    command_arg_bytes: 0,
                    resource_id: Some(opaque_resource_id(session_id)),
                    resource_kind: None,
                    child_count: 0,
                    lifecycle: crate::state::ProcessResourceLifecycle::Failed,
                    metrics_status: crate::domain::snapshot::ProcessMetricStatus::Unknown,
                    metric_values: ResourceMetricValueState::Unavailable,
                    cpu_value_state: ResourceMetricValueState::Unavailable,
                    memory_value_state: ResourceMetricValueState::Unavailable,
                    sampling_generation: 0,
                }],
                managed_process_fence: Some(stale_fence.clone()),
                ..Default::default()
            };
            runtime.sessions.insert(session_id.to_string(), session);
        }

        let completion = execute_process_op_inner(
            &manager.inner,
            ProcessOp::KillProcess {
                op_id: next_op_id(),
                session_id: session_id.to_string(),
                pid: running_pid,
                fence: stale_fence,
                response: None,
            },
        );
        assert!(completion.result.is_err());
        assert!(completion.result.unwrap_err().contains("authority"));
        let runtime = manager.runtime_state();
        let session = runtime
            .sessions
            .get(session_id)
            .expect("failed runtime residue remains visible");
        assert!(session.pid.is_none());
        assert!(session.resources.process_ids.is_empty());
        assert!(session.resources.processes.is_empty());
        assert!(session.resources.managed_process_fence.is_none());
    }

    #[test]
    fn missing_exact_owner_is_not_reported_as_a_successful_retry() {
        let manager = ProcessManager::new();
        let session_id = "missing-exact-owner";
        let mut runtime = SessionRuntimeState::new(
            session_id,
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        runtime.status = SessionStatus::Failed;
        runtime.reap_incomplete = true;
        runtime.pid = Some(std::process::id());
        manager.register_runtime_session(runtime);

        let error = retry_exact_session_teardown(&manager.inner, session_id)
            .expect_err("PID absence or a missing session owner cannot forge exact settlement");

        assert!(error.contains("authority"), "{error}");
        let runtime = manager.runtime_state();
        let retained = runtime
            .sessions
            .get(session_id)
            .expect("runtime row retained");
        assert_eq!(retained.status, SessionStatus::Failed);
        assert!(retained.reap_incomplete);
    }

    #[cfg(windows)]
    #[test]
    fn process_op_close_retains_exact_owner_when_teardown_persistence_fails() {
        let cwd = temp_test_dir("close-retains-exact-owner");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "close-retains-exact-owner";
        manager
            .spawn_shell_session(session_id, &cwd, SessionDimensions::default(), None, None)
            .expect("spawn exact managed terminal");
        wait_for_live_session(&manager, session_id);

        let completion_store = manager
            .inner
            .terminal_authority_issuer
            .state
            .lock()
            .expect("terminal authority state")
            .completion_store
            .clone()
            .expect("terminal completion store");
        completion_store.fail_persist_for_test("injected transient persistence failure");

        let failed = execute_process_op_inner(
            &manager.inner,
            ProcessOp::CloseAi {
                op_id: next_op_id(),
                session_id: session_id.to_string(),
                response: None,
            },
        );
        assert!(failed.result.is_err());
        assert!(
            manager.session_attached(session_id),
            "a failed exact close must retain the sole TerminalSession/Job owner"
        );

        completion_store.clear_persist_failure_for_test();
        let retry = execute_process_op_inner(
            &manager.inner,
            ProcessOp::CloseAi {
                op_id: next_op_id(),
                session_id: session_id.to_string(),
                response: None,
            },
        );
        retry.result.expect("exact close retry must settle");
        assert!(!manager.session_attached(session_id));
    }

    #[cfg(windows)]
    #[test]
    fn process_action_closes_only_the_exact_snapshot_generation() {
        let cwd = temp_test_dir("kill-accept-root");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "shell-kill-accept";

        manager
            .spawn_shell_session(session_id, &cwd, SessionDimensions::default(), None, None)
            .unwrap();
        wait_for_live_session(&manager, session_id);
        let pid = manager
            .runtime_state()
            .sessions
            .get(session_id)
            .and_then(|session| session.pid)
            .expect("live pid");
        let session = manager
            .inner
            .sessions
            .lock()
            .expect("session store")
            .get(session_id)
            .cloned()
            .expect("managed terminal session");
        let query = session
            .managed_process_observations_until(Instant::now() + Duration::from_secs(2), 512)
            .expect("exact Job observation")
            .expect("managed teardown authority");
        let (capture, members) = query.into_parts();
        members.expect("exact Job members");
        let fence = capture.fence().clone();

        let completion = execute_process_op_inner(
            &manager.inner,
            ProcessOp::KillProcess {
                op_id: next_op_id(),
                session_id: session_id.to_string(),
                pid,
                fence,
                response: None,
            },
        );
        completion.result.expect("exact snapshot-fenced close");
        assert!(
            !manager.session_attached(session_id),
            "exact close must remove only the selected managed session generation"
        );
    }

    #[test]
    fn note_reap_incomplete_never_recaptures_pid_or_action_authority() {
        let cwd = temp_test_dir("reap-incomplete");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "reap-incomplete-session";

        let identity =
            platform_service::capture_process_identity(std::process::id()).expect("self identity");
        pid_file::track_session_process(pid_file::ManagedProcessRecord {
            session_id: session_id.to_string(),
            pid: identity.pid,
            started_at_unix_secs: identity.started_at_unix_secs,
            process_name: identity.process_name.clone(),
            session_kind: "shell".to_string(),
            program: "test-shell".to_string(),
            project_id: None,
            command_id: None,
            tab_id: None,
            descendant_processes: Vec::new(),
        })
        .unwrap();

        {
            let mut runtime = manager.inner.runtime_state.write().expect("runtime write");
            let mut session = SessionRuntimeState::new(
                session_id,
                cwd.clone(),
                SessionDimensions::default(),
                TerminalBackend::PortablePtyFeedingAlacritty,
            );
            session.status = SessionStatus::Stopping;
            session.pid = Some(identity.pid);
            session.resources.process_count = 1;
            session.resources.process_ids = vec![identity.pid];
            session.resources.managed_process_fence = Some(synthetic_process_fence(identity.pid));
            runtime.sessions.insert(session_id.to_string(), session);
        }

        manager.note_reap_incomplete(session_id);
        let runtime = manager.runtime_state();
        let session = runtime.sessions.get(session_id).expect("session");
        assert!(session.reap_incomplete);
        assert_eq!(session.status, SessionStatus::Failed);
        assert!(session.pid.is_none());
        assert!(session.resources.process_ids.is_empty());
        assert!(session.resources.processes.is_empty());
        assert_eq!(session.resources.process_count, 0);
        assert_eq!(
            session.resources.process_count_value_state,
            ResourceMetricValueState::Unavailable
        );
        assert!(session.resources.managed_process_fence.is_none());
        assert_eq!(
            session.resources.metrics_error.as_deref(),
            Some("exact_teardown_incomplete")
        );
        assert!(session
            .exit
            .as_ref()
            .is_some_and(|exit| exit.summary.contains("retry")));
    }

    fn injected_sampling_fixture(
        manager: &ProcessManager,
        session_id: &str,
        fence: ManagedProcessFence,
    ) -> ResourceSamplingSource {
        let pid = fence.root().id().pid();
        let mut runtime = SessionRuntimeState::new(
            session_id,
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        runtime.status = SessionStatus::Running;
        runtime.pid = Some(pid);
        manager.register_runtime_session(runtime);

        ResourceSamplingSource {
            sessions: HashMap::from([(
                session_id.to_string(),
                ResourceSamplingSession {
                    managed_process_fence: Some(fence.clone()),
                    job_members: vec![JobMemberObservation::Accessible {
                        identity: fence.root().clone(),
                    }],
                    member_observations: vec![ProcessMemberObservation::Accessible(
                        AccessibleProcess::new(fence.root().clone(), 0, 4_096),
                    )],
                    metadata: HashMap::from([(
                        pid,
                        ProcessProjectionMetadata {
                            display_name: "Shell".to_string(),
                            command_label: "Shell".to_string(),
                            ..ProcessProjectionMetadata::default()
                        },
                    )]),
                },
            )]),
            ..ResourceSamplingSource::default()
        }
    }

    fn spawn_sampling_refresh(
        manager: ProcessManager,
        source: Option<ResourceSamplingSource>,
    ) -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Receiver<()>,
        thread::JoinHandle<()>,
    ) {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(1);
        let handle = thread::spawn(move || {
            let mut system = sysinfo::System::new();
            started_tx.send(()).expect("sampling worker started");
            refresh_resource_snapshots_with_source(&manager.inner, &mut system, source.as_ref());
            finished_tx.send(()).expect("sampling worker finished");
        });
        (started_rx, finished_rx, handle)
    }

    #[test]
    fn sampling_tick_does_not_wait_past_one_deadline_for_runtime_session_or_sampler_locks() {
        const COMPLETION_BOUND: Duration = Duration::from_millis(250);

        let runtime_manager = ProcessManager::new();
        let runtime_guard = runtime_manager
            .inner
            .runtime_state
            .write()
            .expect("hold runtime projection");
        let (started, finished, handle) = spawn_sampling_refresh(runtime_manager.clone(), None);
        started.recv().expect("runtime-lock worker started");
        let runtime_bounded = finished.recv_timeout(COMPLETION_BOUND).is_ok();
        drop(runtime_guard);
        handle.join().expect("runtime-lock worker joined");

        let sessions_manager = ProcessManager::new();
        let _ = injected_sampling_fixture(
            &sessions_manager,
            "session-lock-budget",
            sealed_fence_issuer::issue(20, 1, ProcessOwner::Host, 720, 31),
        );
        let sessions_guard = sessions_manager
            .inner
            .sessions
            .lock()
            .expect("hold terminal session store");
        let (started, finished, handle) = spawn_sampling_refresh(sessions_manager.clone(), None);
        started.recv().expect("session-lock worker started");
        let sessions_bounded = finished.recv_timeout(COMPLETION_BOUND).is_ok();
        drop(sessions_guard);
        handle.join().expect("session-lock worker joined");

        let sampler_manager = ProcessManager::new();
        let source = injected_sampling_fixture(
            &sampler_manager,
            "sampler-lock-budget",
            sealed_fence_issuer::issue(21, 1, ProcessOwner::Host, 721, 32),
        );
        let sampler_guard = sampler_manager
            .inner
            .resource_samplers
            .lock()
            .expect("hold sampler store");
        let (started, finished, handle) =
            spawn_sampling_refresh(sampler_manager.clone(), Some(source));
        started.recv().expect("sampler-lock worker started");
        let sampler_bounded = finished.recv_timeout(COMPLETION_BOUND).is_ok();
        drop(sampler_guard);
        handle.join().expect("sampler-lock worker joined");

        assert!(
            runtime_bounded,
            "runtime read exceeded the one tick deadline"
        );
        assert!(
            sessions_bounded,
            "session store exceeded the one tick deadline"
        );
        assert!(
            sampler_bounded,
            "sampler store exceeded the one tick deadline"
        );
    }

    #[test]
    fn expired_tick_never_commits_an_injected_snapshot_after_sampling() {
        let manager = ProcessManager::new();
        let session_id = "expired-direct-publication";
        let fence = sealed_fence_issuer::issue(22, 1, ProcessOwner::Host, 722, 33);
        let mut source = injected_sampling_fixture(&manager, session_id, fence);
        source.before_direct_publication_delay = Some(Duration::from_millis(75));

        let mut system = sysinfo::System::new();
        refresh_resource_snapshots_with_source(&manager.inner, &mut system, Some(&source));

        let runtime = manager.runtime_state();
        let session = runtime.sessions.get(session_id).expect("runtime session");
        assert!(session.resources.last_sample_at.is_none());
        assert!(session.resources.managed_process_fence.is_none());
    }

    #[test]
    fn injected_sampling_rejects_fence_job_and_metric_identity_mismatch() {
        let fence = sealed_fence_issuer::issue(7, 3, ProcessOwner::Host, 700, 11);
        let foreign_member = sealed_fence_issuer::issue(11, 7, ProcessOwner::Host, 701, 22)
            .root()
            .clone();
        let missing_root = ResourceSamplingSession {
            managed_process_fence: Some(fence.clone()),
            job_members: vec![JobMemberObservation::Accessible {
                identity: foreign_member.clone(),
            }],
            member_observations: vec![ProcessMemberObservation::Accessible(
                AccessibleProcess::new(foreign_member, 0, 1),
            )],
            metadata: HashMap::new(),
        };
        let mut budget = SamplingBudget::from_now(2, Duration::from_secs(1));
        assert_eq!(
            clone_injected_job_members_with_budget(&missing_root, 2, &mut budget)
                .expect_err("the injected Job tuple must contain the exact fenced root"),
            SamplerError::ObservationFailed {
                pid: 700,
                reason: "injected_source_missing_exact_root".to_string(),
            }
        );

        let conflicting = sealed_fence_issuer::issue(8, 4, ProcessOwner::Host, 700, 12)
            .root()
            .clone();
        let source = ResourceSamplingSession {
            managed_process_fence: Some(fence.clone()),
            job_members: vec![JobMemberObservation::Accessible {
                identity: conflicting.clone(),
            }],
            member_observations: vec![ProcessMemberObservation::Accessible(
                AccessibleProcess::new(conflicting, 0, 1),
            )],
            metadata: HashMap::new(),
        };
        let mut budget = SamplingBudget::from_now(1, Duration::from_secs(1));
        assert_eq!(
            clone_injected_job_members_with_budget(&source, 1, &mut budget)
                .expect_err("fence and Job root must be one exact identity"),
            SamplerError::ConflictingProcessIdentity { pid: 700 }
        );

        let exact_source = ResourceSamplingSession {
            managed_process_fence: Some(fence.clone()),
            job_members: vec![JobMemberObservation::Accessible {
                identity: fence.root().clone(),
            }],
            member_observations: vec![ProcessMemberObservation::Accessible(
                AccessibleProcess::new(
                    sealed_fence_issuer::issue(9, 5, ProcessOwner::Host, 700, 13)
                        .root()
                        .clone(),
                    0,
                    1,
                ),
            )],
            metadata: HashMap::new(),
        };
        let mut budget = SamplingBudget::from_now(1, Duration::from_secs(1));
        let job_members = clone_injected_job_members_with_budget(&exact_source, 1, &mut budget)
            .expect("exact injected Job tuple");
        assert_eq!(
            clone_injected_member_observations_with_budget(
                &job_members,
                &exact_source.member_observations,
                &mut budget,
            )
            .expect_err("preadmitted PID must still validate metric generation"),
            SamplerError::ConflictingProcessIdentity { pid: 700 }
        );
    }

    #[test]
    fn injected_metadata_is_authoritative_bounded_redacted_and_budgeted() {
        let fence = sealed_fence_issuer::issue(10, 6, ProcessOwner::Host, 710, 21);
        let source_session = ResourceSamplingSession {
            managed_process_fence: Some(fence.clone()),
            job_members: vec![JobMemberObservation::Accessible {
                identity: fence.root().clone(),
            }],
            member_observations: vec![ProcessMemberObservation::Accessible(
                AccessibleProcess::new(fence.root().clone(), 0, 1),
            )],
            metadata: HashMap::from([
                (
                    710,
                    ProcessProjectionMetadata {
                        parent_pid: Some(999),
                        display_name: "secret/".repeat(2_000),
                        command_label: "--token=secret".repeat(2_000),
                        command_arg_count: u16::MAX,
                        command_arg_bytes: u32::MAX,
                        blocking_external_editor: false,
                    },
                ),
                (
                    999,
                    ProcessProjectionMetadata {
                        display_name: "must-not-project".to_string(),
                        ..ProcessProjectionMetadata::default()
                    },
                ),
            ]),
        };
        let mut budget = SamplingBudget::from_now(1, Duration::from_secs(1));
        let members = clone_injected_job_members_with_budget(&source_session, 1, &mut budget)
            .expect("bounded exact Job tuple");
        let observations = HashMap::from([(
            "bounded".to_string(),
            ManagedJobObservationSnapshot {
                capture: None,
                managed_process_fence: Some(fence),
                members: Some(members),
                error: None,
            },
        )]);
        let source = ResourceSamplingSource {
            sessions: HashMap::from([("bounded".to_string(), source_session)]),
            ..ResourceSamplingSource::default()
        };
        let authoritative = BTreeSet::from([710]);
        let metadata =
            capture_injected_process_metadata(&source, &observations, &authoritative, &mut budget);

        assert_eq!(metadata.len(), 1);
        assert!(!metadata.contains_key(&999));
        let row = metadata.get(&710).expect("authoritative metadata");
        assert_eq!(row.display_name, "Other process");
        assert_eq!(row.command_label, "Other process");
        assert_eq!(row.command_arg_count, MAX_COMMAND_ARGUMENTS as u16);
        assert_eq!(row.command_arg_bytes, MAX_COMMAND_ARGUMENT_BYTES as u32);
        assert!(row.parent_pid.is_none());
        assert_eq!(budget.work_counters().metadata_snapshots, 1);
        assert_eq!(budget.work_counters().metadata_rows, 1);

        let mut expired = SamplingBudget::new(Instant::now(), 1);
        assert!(capture_injected_process_metadata(
            &source,
            &observations,
            &authoritative,
            &mut expired,
        )
        .is_empty());
        assert_eq!(expired.work_counters().metadata_rows, 0);
    }

    #[cfg(windows)]
    #[test]
    fn refresh_resource_snapshots_populates_named_process_nodes_and_exact_fence() {
        let cwd = temp_test_dir("resource-sample-nodes");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "shell-sample-nodes";

        manager
            .spawn_shell_session(session_id, &cwd, SessionDimensions::default(), None, None)
            .unwrap();
        wait_for_live_session(&manager, session_id);
        wait_for_tracked_process(session_id);

        // Capture the exact current Job members once, outside the production
        // tick. Windows identity/command queries can exceed 40 ms even for a
        // one-process Job; the projection contract itself is deterministic
        // when fed this bounded immutable source snapshot.
        let managed_session = manager
            .inner
            .sessions
            .lock()
            .expect("session store")
            .get(session_id)
            .cloned()
            .expect("live managed session");
        let query = managed_session
            .managed_process_observations_until(Instant::now() + Duration::from_secs(2), 512)
            .expect("bounded exact Job observation")
            .expect("live managed Job members");
        let (capture, job_members) = query.into_parts();
        let managed_process_fence = capture.fence().clone();
        let job_members = job_members.expect("bounded exact Job members");
        assert!(!job_members.is_empty(), "expected a live Job member");
        let member_observations = job_members
            .iter()
            .map(|member| match member {
                JobMemberObservation::Accessible { identity } => {
                    ProcessMemberObservation::Accessible(AccessibleProcess::new(
                        identity.clone(),
                        0,
                        4_096,
                    ))
                }
                JobMemberObservation::Inaccessible {
                    pid,
                    creation_time_100ns,
                    reason,
                } => ProcessMemberObservation::Inaccessible(
                    InaccessibleProcess::new(*pid, *creation_time_100ns)
                        .with_reason(reason.clone()),
                ),
            })
            .collect::<Vec<_>>();
        let metadata = job_members
            .iter()
            .filter_map(|member| match member {
                JobMemberObservation::Accessible { identity } => Some((
                    identity.id().pid(),
                    ProcessProjectionMetadata {
                        parent_pid: None,
                        display_name: "Shell".to_string(),
                        command_label: "Shell".to_string(),
                        command_arg_count: 0,
                        command_arg_bytes: 0,
                        blocking_external_editor: false,
                    },
                )),
                JobMemberObservation::Inaccessible { .. } => None,
            })
            .collect::<HashMap<_, _>>();
        let source = ResourceSamplingSource {
            sessions: HashMap::from([(
                session_id.to_string(),
                ResourceSamplingSession {
                    managed_process_fence: Some(managed_process_fence.clone()),
                    job_members,
                    member_observations,
                    metadata,
                },
            )]),
            ..ResourceSamplingSource::default()
        };

        let mut system = sysinfo::System::new();
        refresh_resource_snapshots_with_source(&manager.inner, &mut system, Some(&source));

        let session = manager
            .runtime_state()
            .sessions
            .get(session_id)
            .cloned()
            .expect("session");
        assert!(
            !session.resources.processes.is_empty(),
            "expected named process nodes from sampler"
        );
        assert_eq!(
            session.resources.process_count as usize,
            session.resources.processes.len()
        );
        assert!(!session.resources.processes[0].name.is_empty());
        assert_eq!(
            session.resources.managed_process_fence.as_ref(),
            Some(&managed_process_fence),
            "the action fence must come from the same immutable Job observation source"
        );

        let _ = manager.close_session(session_id);
    }

    #[test]
    fn resource_snapshot_processes_round_trip_in_session_state() {
        let mut session = SessionRuntimeState::new(
            "resource-nodes",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.note_resource_sample(ResourceSnapshot {
            cpu_percent: 12.5,
            memory_bytes: 2048,
            process_count: 2,
            process_ids: vec![1, 2],
            processes: vec![
                crate::state::ProcessResourceNode {
                    pid: 1,
                    parent_pid: None,
                    name: "shell".to_string(),
                    cpu_percent: 1.0,
                    core_equivalent_percent: 1.0,
                    memory_bytes: 1024,
                    memory_metric: ResourceMemoryMetric::PrivateResident,
                    creation_time_100ns: None,
                    executable: None,
                    command_label: None,
                    command_arg_count: 0,
                    command_arg_bytes: 0,
                    resource_id: Some("shell-sample-nodes".to_string()),
                    resource_kind: None,
                    child_count: 1,
                    lifecycle: crate::state::ProcessResourceLifecycle::Running,
                    metrics_status: crate::domain::snapshot::ProcessMetricStatus::Complete,
                    metric_values: ResourceMetricValueState::Observed,
                    cpu_value_state: ResourceMetricValueState::Observed,
                    memory_value_state: ResourceMetricValueState::Observed,
                    sampling_generation: 1,
                },
                crate::state::ProcessResourceNode {
                    pid: 2,
                    parent_pid: Some(1),
                    name: "node".to_string(),
                    cpu_percent: 11.5,
                    core_equivalent_percent: 11.5,
                    memory_bytes: 1024,
                    memory_metric: ResourceMemoryMetric::PrivateResident,
                    creation_time_100ns: None,
                    executable: None,
                    command_label: Some("node".to_string()),
                    command_arg_count: 0,
                    command_arg_bytes: 0,
                    resource_id: Some("shell-sample-nodes".to_string()),
                    resource_kind: None,
                    child_count: 0,
                    lifecycle: crate::state::ProcessResourceLifecycle::Running,
                    metrics_status: crate::domain::snapshot::ProcessMetricStatus::Complete,
                    metric_values: ResourceMetricValueState::Observed,
                    cpu_value_state: ResourceMetricValueState::Observed,
                    memory_value_state: ResourceMetricValueState::Observed,
                    sampling_generation: 1,
                },
            ],
            last_sample_at: Some(Instant::now()),
            ..Default::default()
        });
        assert_eq!(session.resources.processes.len(), 2);
        assert_eq!(session.resources.processes[1].name, "node");
    }

    #[test]
    fn logical_cpu_count_uses_the_platform_machine_count() {
        let logical_cpu_count = resolve_logical_cpu_count();

        assert_eq!(
            logical_cpu_count,
            platform_service::logical_processor_count()
        );
    }

    #[test]
    fn production_accounting_tick_expires_after_forty_milliseconds() {
        assert_eq!(RESOURCE_SAMPLE_TICK_BUDGET, Duration::from_millis(40));
    }

    #[test]
    fn process_classifier_uses_safe_known_roles_without_leaking_arguments() {
        assert_eq!(
            classify_process_display_name(
                "node.exe",
                &[
                    "node.exe".to_string(),
                    r"C:\repo\node_modules\tinypool\dist\entry\process.js".to_string(),
                ],
            ),
            "Vitest worker"
        );
        assert_eq!(
            classify_process_display_name(
                "node.exe",
                &[
                    "node.exe".to_string(),
                    r"C:\repo\node_modules\vitest\vitest.mjs".to_string(),
                ],
            ),
            "Vitest"
        );
        assert_eq!(
            classify_process_display_name(
                "node.exe",
                &["node.exe".to_string(), "@upstash/context7-mcp".to_string(),],
            ),
            "Context7 MCP"
        );
        assert_eq!(
            classify_process_display_name(
                "node.exe",
                &["node.exe".to_string(), "npm-cli.js".to_string()],
            ),
            "npm"
        );
        assert_eq!(
            classify_process_display_name(
                "node.exe",
                &["node.exe".to_string(), "npx-cli.js".to_string()],
            ),
            "npx"
        );

        let unknown = classify_process_display_name(
            "node.exe",
            &[
                "node.exe".to_string(),
                "custom-runner.js".to_string(),
                "--token=do-not-render-this".to_string(),
            ],
        );
        assert_eq!(unknown, "Node");
        assert!(!unknown.contains("do-not-render-this"));
    }

    #[test]
    fn ai_acceptance_default_provider_commands_preserve_interactive_questions_and_approvals() {
        let settings = Settings::default();
        let claude =
            resolve_ai_startup_command(&settings, TabType::Claude).expect("Claude command");
        let codex = resolve_ai_startup_command(&settings, TabType::Codex).expect("Codex command");

        assert!(!claude.contains("dangerously-skip-permissions"));
        assert!(!codex.contains("dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn failed_job_query_keeps_immutable_last_known_values_marked_stale() {
        let pid = std::process::id();
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let prior = ResourceSnapshot {
            cpu_percent: 31.5,
            memory_bytes: 4096,
            process_count: 1,
            process_ids: vec![pid],
            processes: vec![crate::state::ProcessResourceNode {
                pid,
                parent_pid: None,
                name: r"C:\private\raw-command --token=secret".to_string(),
                cpu_percent: 31.5,
                core_equivalent_percent: 100.0,
                memory_bytes: 4096,
                memory_metric: ResourceMemoryMetric::PrivateResident,
                creation_time_100ns: None,
                executable: Some(r"C:\private\node.exe".to_string()),
                command_label: Some("Node".to_string()),
                command_arg_count: 2,
                command_arg_bytes: 42,
                resource_id: Some("private-session-id".to_string()),
                resource_kind: Some("terminal".to_string()),
                child_count: 0,
                lifecycle: ProcessResourceLifecycle::Running,
                metrics_status: ProcessMetricStatus::Complete,
                metric_values: ResourceMetricValueState::Observed,
                cpu_value_state: ResourceMetricValueState::Observed,
                memory_value_state: ResourceMetricValueState::Observed,
                sampling_generation: 7,
            }],
            metric_values: ResourceMetricValueState::Observed,
            cpu_value_state: ResourceMetricValueState::Observed,
            memory_value_state: ResourceMetricValueState::Observed,
            managed_process_fence: Some(synthetic_process_fence(pid)),
            ..ResourceSnapshot::default()
        };
        let mut budget = SamplingBudget::from_now(512, Duration::from_secs(1));
        let stale = stale_resource_snapshot(
            &system,
            "private-session-id",
            Some(&prior),
            ResourceSampleContext {
                is_ai_session: false,
                logical_cpu_count: 8,
                sampled_at: Instant::now(),
                resource_kind: SessionKind::Shell,
                lifecycle: ProcessResourceLifecycle::Running,
            },
            Some(r"QueryInformationJobObject failed: C:\secret\token"),
            &mut budget,
        );

        assert!(stale.metrics_stale);
        assert!(
            stale.managed_process_fence.is_none(),
            "a failed current Job query must not preserve prior control authority"
        );
        assert_eq!(stale.metric_values, ResourceMetricValueState::LastKnown);
        assert_eq!(stale.cpu_percent, 31.5);
        assert_eq!(
            stale.processes[0].metric_values,
            ResourceMetricValueState::LastKnown
        );
        assert_eq!(
            stale.processes[0].cpu_value_state,
            ResourceMetricValueState::LastKnown
        );
        assert_eq!(
            stale.processes[0].memory_value_state,
            ResourceMetricValueState::LastKnown
        );
        assert_eq!(
            stale.processes[0].lifecycle,
            ProcessResourceLifecycle::Unknown
        );
        assert_eq!(
            stale.metrics_error.as_deref(),
            Some("job_query_unavailable")
        );
        assert_eq!(stale.processes[0].executable.as_deref(), Some("node.exe"));
        assert_eq!(
            stale.processes[0].name,
            "Other process (metrics unavailable)"
        );
        assert!(stale.processes[0]
            .resource_id
            .as_deref()
            .is_some_and(|id| id.starts_with("resource-")));
    }

    #[test]
    fn stale_aggregate_confidence_comes_from_the_prior_aggregate_not_rows() {
        let prior = ResourceSnapshot {
            cpu_percent: 7.5,
            memory_bytes: 2_048,
            process_count: 9,
            process_count_value_state: ResourceMetricValueState::Observed,
            processes: Vec::new(),
            cpu_value_state: ResourceMetricValueState::Partial,
            memory_value_state: ResourceMetricValueState::Partial,
            metric_values: ResourceMetricValueState::Partial,
            ..ResourceSnapshot::default()
        };
        let mut budget = SamplingBudget::from_now(512, Duration::from_secs(1));
        let stale = stale_resource_snapshot(
            &sysinfo::System::new(),
            "session-with-aggregate-only",
            Some(&prior),
            ResourceSampleContext {
                is_ai_session: false,
                logical_cpu_count: 8,
                sampled_at: Instant::now(),
                resource_kind: SessionKind::Shell,
                lifecycle: ProcessResourceLifecycle::Running,
            },
            Some("C:\\secret\\must-not-escape"),
            &mut budget,
        );

        assert_eq!(stale.cpu_value_state, ResourceMetricValueState::LastKnown);
        assert_eq!(
            stale.memory_value_state,
            ResourceMetricValueState::LastKnown
        );
        assert_eq!(stale.metric_values, ResourceMetricValueState::LastKnown);
        assert_eq!(stale.process_count, 9);
        assert_eq!(
            stale.process_count_value_state,
            ResourceMetricValueState::LastKnown
        );
        assert_eq!(
            stale.metrics_error.as_deref(),
            Some("job_query_unavailable")
        );
    }

    #[test]
    fn cached_snapshot_copy_is_bounded_before_materialization() {
        let prior = ResourceSnapshot {
            process_count: 16_384,
            process_ids: (1..=16_384).collect(),
            managed_process_fence: Some(synthetic_process_fence(1)),
            ..ResourceSnapshot::default()
        };
        let mut budget = SamplingBudget::from_now(512, Duration::from_secs(1));

        let bounded = bounded_previous_snapshot(&prior, &mut budget);

        assert_eq!(bounded.process_ids.len(), 512);
        assert_eq!(bounded.process_count, 512);
        assert_eq!(budget.work_counters().cached_process_ids, 512);
        assert_eq!(budget.work_counters().cached_process_rows, 0);
        assert!(bounded.managed_process_fence.is_none());
    }

    #[test]
    fn expired_budget_does_not_copy_cached_process_vectors() {
        let prior = ResourceSnapshot {
            process_count: 2,
            process_ids: vec![1, 2],
            managed_process_fence: Some(synthetic_process_fence(1)),
            ..ResourceSnapshot::default()
        };
        let mut budget = SamplingBudget::new(Instant::now(), 512);

        let bounded = bounded_previous_snapshot(&prior, &mut budget);

        assert!(bounded.process_ids.is_empty());
        assert!(bounded.processes.is_empty());
        assert!(bounded.managed_process_fence.is_none());
        assert_eq!(budget.work_counters().cached_process_ids, 0);
    }

    #[test]
    fn job_query_limit_uses_global_remaining_members_and_skips_at_zero() {
        let mut budget = SamplingBudget::from_now(2, Duration::from_secs(1));
        assert_eq!(job_query_member_limit(&budget).expect("initial limit"), 2);

        budget
            .admit_identity(synthetic_process_fence(1).root())
            .expect("first exact member");
        assert_eq!(job_query_member_limit(&budget).expect("remaining limit"), 1);

        budget
            .admit_identity(synthetic_process_fence(2).root())
            .expect("second exact member");
        let error = job_query_member_limit(&budget)
            .expect_err("zero remaining capacity must skip the next Job query");
        assert!(matches!(error, SamplerError::WorkBudgetExceeded { .. }));
    }

    #[test]
    fn failed_metric_projection_never_mints_control_authority() {
        let snapshot = budget_failed_resource_snapshot(
            "session",
            &[],
            ResourceSampleContext {
                is_ai_session: false,
                logical_cpu_count: 8,
                sampled_at: Instant::now(),
                resource_kind: SessionKind::Shell,
                lifecycle: ProcessResourceLifecycle::Running,
            },
            SamplerError::WorkBudgetExceeded {
                attempted: 513,
                max: 512,
            },
        );

        assert_eq!(snapshot.metrics_status, ProcessMetricStatus::Failed);
        assert!(snapshot.managed_process_fence.is_none());
    }

    #[test]
    fn inaccessible_or_vanished_members_have_truthful_lifecycle() {
        let inaccessible = JobMemberObservation::Inaccessible {
            pid: std::process::id(),
            creation_time_100ns: None,
            reason: "access_denied".to_string(),
        };
        assert_eq!(
            process_resource_lifecycle(ProcessResourceLifecycle::Running, Some(&inaccessible),),
            ProcessResourceLifecycle::Unknown
        );

        let exact_identity = ManagedProcessIdentity::new(
            crate::process::identity::ManagedProcessId::new(std::process::id(), 1)
                .expect("test process id"),
            std::env::current_exe().expect("test executable"),
        )
        .expect("canonical test executable");
        let accessible = JobMemberObservation::Accessible {
            identity: exact_identity,
        };
        assert_eq!(
            process_resource_lifecycle(ProcessResourceLifecycle::Running, Some(&accessible),),
            ProcessResourceLifecycle::Running,
            "an exact current Job member retains session lifecycle even before a CPU baseline"
        );
        assert_eq!(
            process_resource_lifecycle(ProcessResourceLifecycle::Running, None,),
            ProcessResourceLifecycle::Unknown
        );
    }

    fn wait_for_live_session(manager: &ProcessManager, session_id: &str) {
        for _ in 0..50 {
            let _ = manager.drain_process_op_completions();
            if manager
                .runtime_state()
                .sessions
                .get(session_id)
                .map(|session| session.status.is_live())
                .unwrap_or(false)
                && manager.get_session(session_id).is_ok()
            {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("session `{session_id}` never became live");
    }

    fn wait_for_running_session(manager: &ProcessManager, session_id: &str) {
        let mut operation_completed = false;
        for _ in 0..30 {
            for completion in manager.drain_process_op_completions() {
                if completion.target_id == session_id
                    && completion.kind == ProcessOpKind::StartServer
                {
                    completion
                        .result
                        .unwrap_or_else(|error| panic!("session operation failed: {error}"));
                    operation_completed = true;
                }
            }
            if manager
                .runtime_state()
                .sessions
                .get(session_id)
                .is_some_and(|session| {
                    session.status == SessionStatus::Running && session.pid.is_some()
                })
                && operation_completed
            {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("session `{session_id}` never became fully running");
    }

    fn wait_for_stopped_session(manager: &ProcessManager, session_id: &str) {
        for _ in 0..30 {
            if manager
                .runtime_state()
                .sessions
                .get(session_id)
                .is_some_and(|session| session.status == SessionStatus::Stopped)
            {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("session `{session_id}` never became stopped");
    }

    fn wait_for_tracked_process(session_id: &str) {
        for _ in 0..20 {
            if pid_file::tracked_processes()
                .into_iter()
                .any(|entry| entry.session_id == session_id)
            {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("session `{session_id}` was never tracked");
    }

    fn wait_for_tracked_processes_to_clear() {
        for _ in 0..20 {
            if pid_file::tracked_processes().is_empty() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("tracked process ledger never cleared");
    }

    fn screen_text(view: &TerminalSessionView) -> String {
        view.screen
            .lines
            .iter()
            .map(|line| {
                let mut text: String = line
                    .iter()
                    .map(|cell| {
                        if cell.character == '\u{00a0}' {
                            ' '
                        } else {
                            cell.character
                        }
                    })
                    .collect();
                while text.ends_with(' ') {
                    text.pop();
                }
                text
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[cfg(windows)]
    fn server_test_command() -> (String, Vec<String>) {
        (
            "ping".to_string(),
            vec!["127.0.0.1".to_string(), "-n".to_string(), "6".to_string()],
        )
    }

    #[cfg(not(windows))]
    fn server_test_command() -> (String, Vec<String>) {
        ("sleep".to_string(), vec!["5".to_string()])
    }
}
