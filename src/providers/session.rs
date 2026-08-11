//! Provider runtime generations and presentation views.
//!
//! This module owns the host-side provider session contract.  It deliberately
//! does not spawn a provider executable.  A production launcher must adapt the
//! Task 3.4 managed process/PTY service and return an authenticated lease for
//! the exact Job/fence it registered.  The fake launchers in tests implement
//! the same lease and settlement traits; they do not bypass the contract with
//! a caller-selected PID.

use crate::domain::operation::ResourceFence;
use crate::domain::{
    AgentRole, AgentSessionFacts, AgentSessionId, ProviderSessionId, ResourceId, TaskId, TerminalId,
};
use crate::process::identity::{ManagedProcessId, ProcessOwner};
use crate::process::registry::ManagedProcessFence;
use crate::providers::capabilities::{ProviderCapabilities, ProviderExecutable, ProviderKind};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Weak,
};
use uuid::Uuid;

pub const MAX_SEMANTIC_PROVIDER_VIEWS: usize = 8;
pub const MAX_PROVIDER_LAUNCH_ARGUMENTS: usize = 128;
pub const MAX_PROVIDER_LAUNCH_ENVIRONMENT: usize = 128;

/// Opaque correlation material issued once for every provider process
/// generation.  It is never derived from cwd, timestamps, transcript names,
/// or provider output.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct LaunchNonce(Uuid);

impl LaunchNonce {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for LaunchNonce {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for LaunchNonce {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LaunchNonce(<opaque>)")
    }
}

/// Exact provider/task/session/generation/nonce tuple carried by provider
/// hooks and view subscriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeCorrelation {
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    provider_kind: ProviderKind,
    generation: u64,
    launch_nonce: LaunchNonce,
}

impl RuntimeCorrelation {
    pub const fn task_id(self) -> TaskId {
        self.task_id
    }

    pub const fn agent_session_id(self) -> AgentSessionId {
        self.agent_session_id
    }

    pub const fn provider_kind(self) -> ProviderKind {
        self.provider_kind
    }

    pub const fn generation(self) -> u64 {
        self.generation
    }

    pub const fn launch_nonce(self) -> LaunchNonce {
        self.launch_nonce
    }

    /// Test-only construction aid for a wrong-nonce fixture.  Production
    /// callers receive correlation values from the runtime and cannot choose
    /// the nonce used to launch a generation.
    #[cfg(test)]
    #[doc(hidden)]
    pub const fn set_launch_nonce_for_test(mut self, launch_nonce: LaunchNonce) -> Self {
        self.launch_nonce = launch_nonce;
        self
    }
}

/// The only launch choices owned by this seam.  `Open` is resolved against the
/// persisted exact provider ID by [`StartProviderSessionRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSessionStartMode {
    Open,
    NewConversation,
    ResumeExact,
}

/// Concrete provider launch mode after resolving a start request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderLaunchMode {
    NewConversation,
    ResumeExact(ProviderSessionId),
}

/// A launch plan sealed by the host for one runtime generation.
///
/// The fields are private on purpose.  The launcher receives this value from
/// [`ProviderRuntimeLaunchRequest`] and cannot substitute a path, task,
/// resource, generation, cwd, or environment after the manager has admitted
/// the request.  A future adapter should provide the provider-specific
/// arguments through the crate-private adapter handoff rather than accepting
/// arbitrary caller strings here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLaunchSpec {
    provider_kind: ProviderKind,
    executable: ProviderExecutable,
    mode: ProviderLaunchMode,
    arguments: Vec<OsString>,
    cwd: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    task_id: TaskId,
    resource_id: ResourceId,
    terminal_id: TerminalId,
    generation: u64,
    launch_nonce: LaunchNonce,
}

impl ProviderLaunchSpec {
    pub const fn provider_kind(&self) -> ProviderKind {
        self.provider_kind
    }

    pub fn executable(&self) -> &ProviderExecutable {
        &self.executable
    }

    pub fn mode(&self) -> &ProviderLaunchMode {
        &self.mode
    }

    pub fn arguments(&self) -> impl Iterator<Item = &OsString> {
        self.arguments.iter()
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.environment
    }

    pub const fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub const fn resource_id(&self) -> ResourceId {
        self.resource_id
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn launch_nonce(&self) -> LaunchNonce {
        self.launch_nonce
    }

    pub const fn terminal_id(&self) -> TerminalId {
        self.terminal_id
    }
}

/// Launch request passed to the injected managed-process launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRuntimeLaunchRequest {
    correlation: RuntimeCorrelation,
    launch_spec: ProviderLaunchSpec,
    capabilities: ProviderCapabilities,
}

impl ProviderRuntimeLaunchRequest {
    pub fn correlation(&self) -> RuntimeCorrelation {
        self.correlation
    }

    pub fn launch_spec(&self) -> &ProviderLaunchSpec {
        &self.launch_spec
    }

    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    pub const fn provider_kind(&self) -> ProviderKind {
        self.correlation.provider_kind
    }

    pub const fn resource_id(&self) -> ResourceId {
        self.launch_spec.resource_id
    }

    pub const fn terminal_id(&self) -> TerminalId {
        self.launch_spec.terminal_id
    }
}

/// Managed process identity exposed to views and diagnostics.  It contains a
/// validated PID plus creation time and cannot be fabricated from zero or a
/// bare caller-supplied PID through this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProcessId(ManagedProcessId);

impl ProviderProcessId {
    pub fn pid(&self) -> u32 {
        self.0.pid()
    }

    pub fn creation_time_100ns(&self) -> u64 {
        self.0.creation_time_100ns()
    }

    const fn from_managed(id: ManagedProcessId) -> Self {
        Self(id)
    }
}

/// A launcher-returned, non-forgeable process lease.  Implementations must
/// retain the Job/PTY owner in this value until a joined zero settlement has
/// been returned.  The manager never accepts a raw PID or a copyable process
/// token as lifecycle authority.
pub trait ProviderProcessLease: Send {
    fn fence(&self) -> &ManagedProcessFence;

    fn process_id(&self) -> ProviderProcessId {
        ProviderProcessId::from_managed(self.fence().root().id())
    }
}

/// Typed proof returned only after the managed process tree has joined and its
/// Job reported ACTIVE_PROCESS_ZERO.  A launcher must validate the exact fence
/// before returning this proof.
pub trait ActiveProcessZeroSettlement: Send {
    fn fence(&self) -> &ManagedProcessFence;

    fn is_joined_active_process_zero(&self) -> bool;
}

/// Launch/teardown failures stay typed so exact-resume failures remain visible
/// and cannot be silently retried as a fresh conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExactResumeFailure {
    NotFound,
    Incompatible,
    AuthRequired,
    Unsupported,
    ProviderRejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderLaunchError {
    ExactResumeFailed(ExactResumeFailure),
    SpawnFailed,
    ProcessExited,
    Unsupported,
    StopFailed,
    ProcessFenceMismatch,
    ZeroProcessId,
    ActiveProcessZeroRequired,
}

/// Explicit integration seam for Task 3.4/4.1b.  The production
/// implementation must return its registry-issued managed lease and a typed
/// joined zero settlement.  There is deliberately no stock-CLI or PID-based
/// implementation in this module.
pub trait ProviderProcessLauncher {
    type Lease: ProviderProcessLease;
    type Settlement: ActiveProcessZeroSettlement;

    fn launch(
        &mut self,
        request: &ProviderRuntimeLaunchRequest,
    ) -> Result<Self::Lease, ProviderLaunchError>;

    fn stop_and_join(
        &mut self,
        lease: &mut Self::Lease,
    ) -> Result<Self::Settlement, ProviderLaunchError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartProviderSessionRequest {
    agent: AgentSessionFacts,
    executable: ProviderExecutable,
    capabilities: ProviderCapabilities,
    mode: ProviderSessionStartMode,
}

impl StartProviderSessionRequest {
    pub fn new(
        agent: AgentSessionFacts,
        executable: ProviderExecutable,
        capabilities: ProviderCapabilities,
        mode: ProviderSessionStartMode,
    ) -> Self {
        Self {
            agent,
            executable,
            capabilities,
            mode,
        }
    }

    pub fn open(
        agent: AgentSessionFacts,
        executable: ProviderExecutable,
        capabilities: ProviderCapabilities,
    ) -> Self {
        Self::new(
            agent,
            executable,
            capabilities,
            ProviderSessionStartMode::Open,
        )
    }

    pub fn new_conversation(
        agent: AgentSessionFacts,
        executable: ProviderExecutable,
        capabilities: ProviderCapabilities,
    ) -> Self {
        Self::new(
            agent,
            executable,
            capabilities,
            ProviderSessionStartMode::NewConversation,
        )
    }

    pub fn resume_exact(
        agent: AgentSessionFacts,
        executable: ProviderExecutable,
        capabilities: ProviderCapabilities,
    ) -> Self {
        Self::new(
            agent,
            executable,
            capabilities,
            ProviderSessionStartMode::ResumeExact,
        )
    }

    pub fn agent(&self) -> &AgentSessionFacts {
        &self.agent
    }

    pub fn executable(&self) -> &ProviderExecutable {
        &self.executable
    }

    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    pub const fn mode(&self) -> ProviderSessionStartMode {
        self.mode
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeLifecycle {
    Running,
    Stopping,
    Exited,
    Replaced,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderIdentityState {
    Pending,
    Expected(ProviderSessionId),
    Accepted(ProviderSessionId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderIdentityAcceptance {
    Accepted,
    AlreadyAccepted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderViewKind {
    RawTerminal,
    Semantic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSessionError {
    AgentSessionNotFound(AgentSessionId),
    SessionAlreadyRunning(AgentSessionId),
    SessionClosed(AgentSessionId),
    ExplicitNewConversationRequired {
        agent_session_id: AgentSessionId,
    },
    CorrelatedProviderSessionRequired {
        agent_session_id: AgentSessionId,
    },
    ProviderKindMismatch {
        agent: ProviderKind,
        capabilities: ProviderKind,
    },
    InvalidCapabilities,
    GenerationExhausted,
    LaunchFailed(ProviderLaunchError),
    ExactResumeFailed {
        provider_session_id: ProviderSessionId,
        failure: ExactResumeFailure,
    },
    StopFailed(ProviderLaunchError),
    SettlementRequired {
        agent_session_id: AgentSessionId,
        generation: u64,
    },
    SettlementFenceMismatch,
    StateStore(String),
    StalePersistedState {
        generation: u64,
    },
    StaleAgentFacts {
        generation: u64,
    },
    WrongTask {
        expected: TaskId,
        actual: TaskId,
    },
    WrongAgentSession {
        expected: AgentSessionId,
        actual: AgentSessionId,
    },
    WrongProviderKind {
        expected: ProviderKind,
        actual: ProviderKind,
    },
    StaleGeneration {
        expected: u64,
        actual: u64,
    },
    WrongLaunchNonce,
    RuntimeNotLive {
        lifecycle: RuntimeLifecycle,
    },
    UntrustedSessionStart,
    SessionStartProvenanceMismatch,
    ProviderSessionIdRebind {
        existing: ProviderSessionId,
        received: ProviderSessionId,
    },
    TerminalViewAlreadyAttached,
    SemanticViewLimitReached,
    ViewNotAttached,
    ViewFromDifferentRuntime,
}

impl fmt::Display for ProviderSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AgentSessionNotFound(_) => formatter.write_str("agent session is not registered"),
            Self::SessionAlreadyRunning(_) => {
                formatter.write_str("agent session already has a live generation")
            }
            Self::SessionClosed(_) => formatter.write_str("agent session is closed"),
            Self::ExplicitNewConversationRequired { .. } => formatter.write_str(
                "an agent without an exact provider ID requires explicit new conversation",
            ),
            Self::CorrelatedProviderSessionRequired { .. } => {
                formatter.write_str("exact resume requires a correlated provider session ID")
            }
            Self::ProviderKindMismatch { .. } => {
                formatter.write_str("provider capability kind does not match the agent")
            }
            Self::InvalidCapabilities => {
                formatter.write_str("provider capability snapshot is invalid")
            }
            Self::GenerationExhausted => {
                formatter.write_str("provider runtime generation exhausted")
            }
            Self::LaunchFailed(error) => write!(formatter, "provider launch failed: {error:?}"),
            Self::ExactResumeFailed { failure, .. } => {
                write!(formatter, "exact provider resume failed: {failure:?}")
            }
            Self::StopFailed(error) => write!(formatter, "provider process stop failed: {error:?}"),
            Self::SettlementRequired { .. } => formatter.write_str(
                "provider process tree has not reached a joined ACTIVE_PROCESS_ZERO settlement",
            ),
            Self::SettlementFenceMismatch => {
                formatter.write_str("provider settlement does not match the managed process fence")
            }
            Self::StateStore(error) => {
                write!(formatter, "provider session state store failed: {error}")
            }
            Self::StalePersistedState { .. } => {
                formatter.write_str("persisted provider session state is stale")
            }
            Self::StaleAgentFacts { .. } => formatter.write_str("provider start facts are stale"),
            Self::WrongTask { .. } => formatter.write_str("provider correlation task ID is wrong"),
            Self::WrongAgentSession { .. } => {
                formatter.write_str("provider correlation agent ID is wrong")
            }
            Self::WrongProviderKind { .. } => {
                formatter.write_str("provider correlation provider kind is wrong")
            }
            Self::StaleGeneration { .. } => {
                formatter.write_str("provider correlation generation is stale")
            }
            Self::WrongLaunchNonce => {
                formatter.write_str("provider correlation launch nonce is wrong")
            }
            Self::RuntimeNotLive { .. } => {
                formatter.write_str("provider runtime generation is not live")
            }
            Self::UntrustedSessionStart => formatter.write_str(
                "provider SessionStart requires authenticated current-generation provenance",
            ),
            Self::SessionStartProvenanceMismatch => formatter.write_str(
                "provider SessionStart provenance does not match the runtime generation",
            ),
            Self::ProviderSessionIdRebind { .. } => {
                formatter.write_str("provider session ID attempted to rebind")
            }
            Self::TerminalViewAlreadyAttached => {
                formatter.write_str("raw terminal view is already attached")
            }
            Self::SemanticViewLimitReached => {
                formatter.write_str("semantic provider view limit reached")
            }
            Self::ViewNotAttached => formatter.write_str("provider view is not attached"),
            Self::ViewFromDifferentRuntime => {
                formatter.write_str("provider view belongs to another runtime")
            }
        }
    }
}

impl std::error::Error for ProviderSessionError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistedRuntimeLifecycle {
    Starting,
    Running,
    Stopping,
    Replaced,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSessionState {
    agent_session_id: AgentSessionId,
    task_id: TaskId,
    generation: u64,
    revision: u64,
    lifecycle: PersistedRuntimeLifecycle,
    launch_nonce: LaunchNonce,
}

impl ProviderSessionState {
    pub const fn agent_session_id(&self) -> AgentSessionId {
        self.agent_session_id
    }
    pub const fn task_id(&self) -> TaskId {
        self.task_id
    }
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    pub const fn revision(&self) -> u64 {
        self.revision
    }
    pub const fn lifecycle(&self) -> PersistedRuntimeLifecycle {
        self.lifecycle
    }
    pub const fn launch_nonce(&self) -> LaunchNonce {
        self.launch_nonce
    }
}

/// Durable state authority.  Implementations must make a write visible before
/// the corresponding lifecycle transition is exposed to callers.  Production
/// code should implement this over the Task store; the in-memory implementation
/// exists only as a deterministic fixture.
pub trait ProviderSessionStateStore {
    fn load(
        &self,
        agent_session_id: AgentSessionId,
    ) -> Result<Option<ProviderSessionState>, String>;
    fn persist(&mut self, state: ProviderSessionState) -> Result<(), String>;
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryProviderSessionStateStore {
    states: HashMap<AgentSessionId, ProviderSessionState>,
}

impl InMemoryProviderSessionStateStore {
    pub fn state(&self, agent_session_id: AgentSessionId) -> Option<&ProviderSessionState> {
        self.states.get(&agent_session_id)
    }
}

impl ProviderSessionStateStore for InMemoryProviderSessionStateStore {
    fn load(
        &self,
        agent_session_id: AgentSessionId,
    ) -> Result<Option<ProviderSessionState>, String> {
        Ok(self.states.get(&agent_session_id).cloned())
    }

    fn persist(&mut self, state: ProviderSessionState) -> Result<(), String> {
        if self
            .states
            .get(&state.agent_session_id)
            .is_some_and(|current| state.revision <= current.revision)
        {
            return Err("provider session state revision is not monotonic".to_string());
        }
        self.states.insert(state.agent_session_id, state);
        Ok(())
    }
}

/// Authenticated, current-generation SessionStart provenance.  The fields
/// cannot be constructed by callers.  Task 4.1b should mint this through the
/// crate-private constructor after its nonce/authentication/current-generation
/// checks; the public manager accepts no raw provider ID as lifecycle evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSessionStartProvenance {
    correlation: RuntimeCorrelation,
    provider_session_id: ProviderSessionId,
    authenticated: bool,
}

impl ProviderSessionStartProvenance {
    pub fn correlation(&self) -> RuntimeCorrelation {
        self.correlation
    }
    pub fn provider_session_id(&self) -> &ProviderSessionId {
        &self.provider_session_id
    }

    /// Crate-private handoff for the authenticated Task 4.1b hook registries.
    /// No public fake constructor exists; unit tests in this module use the
    /// same authority seam under `cfg(test)`.
    pub(crate) fn from_authenticated_current_generation(
        correlation: RuntimeCorrelation,
        provider_session_id: ProviderSessionId,
    ) -> Self {
        Self {
            correlation,
            provider_session_id,
            authenticated: true,
        }
    }
}

#[derive(Debug)]
struct RuntimeState {
    correlation: RuntimeCorrelation,
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    role: AgentRole,
    provider_kind: ProviderKind,
    resource_id: ResourceId,
    terminal_id: TerminalId,
    executable: ProviderExecutable,
    capabilities: ProviderCapabilities,
    fence: ManagedProcessFence,
    lifecycle: RuntimeLifecycle,
    root_exit_observed: bool,
    identity: ProviderIdentityState,
    terminal_view_id: Option<u64>,
    semantic_view_ids: HashSet<u64>,
}

/// Shared immutable runtime identity with synchronized lifecycle/identity
/// state.  Clones refer to the same provider process generation; no clone owns
/// the managed process lease.
#[derive(Clone, Debug)]
pub struct ProviderRuntime {
    state: Arc<Mutex<RuntimeState>>,
}

impl ProviderRuntime {
    fn new(
        request: &ProviderRuntimeLaunchRequest,
        agent: &AgentSessionFacts,
        fence: ManagedProcessFence,
        identity: ProviderIdentityState,
        terminal_id: TerminalId,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(RuntimeState {
                correlation: request.correlation,
                task_id: agent.task_id,
                agent_session_id: agent.id,
                role: agent.role.clone(),
                provider_kind: request.correlation.provider_kind,
                resource_id: request.launch_spec.resource_id,
                terminal_id,
                executable: request.launch_spec.executable.clone(),
                capabilities: request.capabilities.clone(),
                fence,
                lifecycle: RuntimeLifecycle::Running,
                root_exit_observed: false,
                identity,
                terminal_view_id: None,
                semantic_view_ids: HashSet::new(),
            })),
        }
    }

    pub fn correlation(&self) -> RuntimeCorrelation {
        self.state.lock().unwrap().correlation
    }
    pub fn task_id(&self) -> TaskId {
        self.state.lock().unwrap().task_id
    }
    pub fn role(&self) -> AgentRole {
        self.state.lock().unwrap().role.clone()
    }
    pub fn provider_kind(&self) -> ProviderKind {
        self.state.lock().unwrap().provider_kind
    }
    pub fn resource_id(&self) -> ResourceId {
        self.state.lock().unwrap().resource_id
    }
    pub fn terminal_id(&self) -> TerminalId {
        self.state.lock().unwrap().terminal_id
    }
    pub fn executable(&self) -> ProviderExecutable {
        self.state.lock().unwrap().executable.clone()
    }
    pub fn capabilities(&self) -> ProviderCapabilities {
        self.state.lock().unwrap().capabilities.clone()
    }
    pub fn fence(&self) -> ManagedProcessFence {
        self.state.lock().unwrap().fence.clone()
    }
    pub fn process_id(&self) -> ProviderProcessId {
        ProviderProcessId::from_managed(self.fence().root().id())
    }
    pub fn launch_nonce(&self) -> LaunchNonce {
        self.correlation().launch_nonce()
    }
    pub fn generation(&self) -> u64 {
        self.correlation().generation()
    }
    pub fn lifecycle(&self) -> RuntimeLifecycle {
        self.state.lock().unwrap().lifecycle
    }
    pub fn root_exit_observed(&self) -> bool {
        self.state.lock().unwrap().root_exit_observed
    }
    pub fn identity_state(&self) -> ProviderIdentityState {
        self.state.lock().unwrap().identity.clone()
    }

    pub fn provider_session_id(&self) -> Option<ProviderSessionId> {
        match self.identity_state() {
            ProviderIdentityState::Pending => None,
            ProviderIdentityState::Expected(id) | ProviderIdentityState::Accepted(id) => Some(id),
        }
    }

    pub fn validate_correlation(
        &self,
        correlation: RuntimeCorrelation,
    ) -> Result<(), ProviderSessionError> {
        let state = self.state.lock().unwrap();
        validate_correlation(&state, correlation)
    }

    fn accept_provider_session_start(
        &self,
        provenance: &ProviderSessionStartProvenance,
    ) -> Result<ProviderIdentityAcceptance, ProviderSessionError> {
        let mut state = self.state.lock().unwrap();
        if !provenance.authenticated {
            return Err(ProviderSessionError::UntrustedSessionStart);
        }
        validate_correlation(&state, provenance.correlation)?;
        if state.lifecycle != RuntimeLifecycle::Running {
            return Err(ProviderSessionError::RuntimeNotLive {
                lifecycle: state.lifecycle,
            });
        }
        match &state.identity {
            ProviderIdentityState::Pending => {
                state.identity =
                    ProviderIdentityState::Accepted(provenance.provider_session_id.clone());
                Ok(ProviderIdentityAcceptance::Accepted)
            }
            ProviderIdentityState::Expected(expected)
                if expected == &provenance.provider_session_id =>
            {
                state.identity =
                    ProviderIdentityState::Accepted(provenance.provider_session_id.clone());
                Ok(ProviderIdentityAcceptance::Accepted)
            }
            ProviderIdentityState::Expected(expected) => {
                Err(ProviderSessionError::ProviderSessionIdRebind {
                    existing: expected.clone(),
                    received: provenance.provider_session_id.clone(),
                })
            }
            ProviderIdentityState::Accepted(existing)
                if existing == &provenance.provider_session_id =>
            {
                Ok(ProviderIdentityAcceptance::AlreadyAccepted)
            }
            ProviderIdentityState::Accepted(existing) => {
                Err(ProviderSessionError::ProviderSessionIdRebind {
                    existing: existing.clone(),
                    received: provenance.provider_session_id.clone(),
                })
            }
        }
    }

    fn attach_terminal_view(&self, view_id: u64) -> Result<ProviderView, ProviderSessionError> {
        let mut state = self.state.lock().unwrap();
        if state.lifecycle != RuntimeLifecycle::Running {
            return Err(ProviderSessionError::RuntimeNotLive {
                lifecycle: state.lifecycle,
            });
        }
        if state.terminal_view_id.is_some() {
            return Err(ProviderSessionError::TerminalViewAlreadyAttached);
        }
        state.terminal_view_id = Some(view_id);
        Ok(ProviderView::new(
            &self.state,
            &state,
            view_id,
            ProviderViewKind::RawTerminal,
        ))
    }

    fn attach_semantic_view(&self, view_id: u64) -> Result<ProviderView, ProviderSessionError> {
        let mut state = self.state.lock().unwrap();
        if state.lifecycle != RuntimeLifecycle::Running {
            return Err(ProviderSessionError::RuntimeNotLive {
                lifecycle: state.lifecycle,
            });
        }
        if state.semantic_view_ids.len() >= MAX_SEMANTIC_PROVIDER_VIEWS {
            return Err(ProviderSessionError::SemanticViewLimitReached);
        }
        state.semantic_view_ids.insert(view_id);
        Ok(ProviderView::new(
            &self.state,
            &state,
            view_id,
            ProviderViewKind::Semantic,
        ))
    }

    fn detach_view(&self, view: &ProviderView) -> Result<(), ProviderSessionError> {
        let mut state = self.state.lock().unwrap();
        if state.correlation != view.correlation || state.fence != view.fence {
            return Err(ProviderSessionError::ViewFromDifferentRuntime);
        }
        if !view.active.swap(false, Ordering::AcqRel) {
            return Err(ProviderSessionError::ViewNotAttached);
        }
        let removed = match view.kind {
            ProviderViewKind::RawTerminal => state
                .terminal_view_id
                .take()
                .is_some_and(|id| id == view.view_id),
            ProviderViewKind::Semantic => state.semantic_view_ids.remove(&view.view_id),
        };
        if removed {
            Ok(())
        } else {
            Err(ProviderSessionError::ViewNotAttached)
        }
    }

    fn mark_root_exit_observed(&self) -> Result<(), ProviderSessionError> {
        let mut state = self.state.lock().unwrap();
        if state.lifecycle != RuntimeLifecycle::Running {
            return Err(ProviderSessionError::RuntimeNotLive {
                lifecycle: state.lifecycle,
            });
        }
        state.root_exit_observed = true;
        Ok(())
    }

    fn mark_stopping(&self) -> Result<(), ProviderSessionError> {
        let mut state = self.state.lock().unwrap();
        match state.lifecycle {
            RuntimeLifecycle::Running => {
                state.lifecycle = RuntimeLifecycle::Stopping;
                Ok(())
            }
            RuntimeLifecycle::Stopping => Ok(()),
            lifecycle => Err(ProviderSessionError::RuntimeNotLive { lifecycle }),
        }
    }

    fn mark_replaced(&self) {
        let mut state = self.state.lock().unwrap();
        state.lifecycle = RuntimeLifecycle::Replaced;
        state.terminal_view_id = None;
        state.semantic_view_ids.clear();
    }

    fn mark_closed(&self) {
        let mut state = self.state.lock().unwrap();
        state.lifecycle = RuntimeLifecycle::Closed;
        state.terminal_view_id = None;
        state.semantic_view_ids.clear();
    }
}

impl ProviderRuntime {
    pub fn agent_session_id(&self) -> AgentSessionId {
        self.state.lock().unwrap().agent_session_id
    }
}

/// A view is a bounded RAII attachment, never process ownership.  It is not
/// `Copy`; dropping it removes its subscription from the generation.  The
/// process lease remains in the manager until an explicit joined settlement.
pub struct ProviderView {
    correlation: RuntimeCorrelation,
    fence: ManagedProcessFence,
    kind: ProviderViewKind,
    view_id: u64,
    active: Arc<AtomicBool>,
    runtime: Weak<Mutex<RuntimeState>>,
}

impl fmt::Debug for ProviderView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderView")
            .field("generation", &self.correlation.generation)
            .field("kind", &self.kind)
            .field("active", &self.active.load(Ordering::Acquire))
            .finish()
    }
}

impl ProviderView {
    fn new(
        runtime: &Arc<Mutex<RuntimeState>>,
        state: &RuntimeState,
        view_id: u64,
        kind: ProviderViewKind,
    ) -> Self {
        Self {
            correlation: state.correlation,
            fence: state.fence.clone(),
            kind,
            view_id,
            active: Arc::new(AtomicBool::new(true)),
            runtime: Arc::downgrade(runtime),
        }
    }

    pub const fn correlation(&self) -> RuntimeCorrelation {
        self.correlation
    }
    pub fn process_id(&self) -> ProviderProcessId {
        ProviderProcessId::from_managed(self.fence.root().id())
    }
    pub const fn kind(&self) -> ProviderViewKind {
        self.kind
    }
    pub const fn view_id(&self) -> u64 {
        self.view_id
    }
    pub const fn agent_session_id(&self) -> AgentSessionId {
        self.correlation.agent_session_id()
    }
    pub const fn task_id(&self) -> TaskId {
        self.correlation.task_id()
    }
    pub const fn provider_kind(&self) -> ProviderKind {
        self.correlation.provider_kind()
    }
    pub const fn generation(&self) -> u64 {
        self.correlation.generation()
    }
    pub const fn launch_nonce(&self) -> LaunchNonce {
        self.correlation.launch_nonce()
    }
}

impl Drop for ProviderView {
    fn drop(&mut self) {
        if !self.active.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some(runtime) = self.runtime.upgrade() {
            let mut state = runtime.lock().unwrap();
            match self.kind {
                ProviderViewKind::RawTerminal => {
                    if state.terminal_view_id == Some(self.view_id) {
                        state.terminal_view_id = None;
                    }
                }
                ProviderViewKind::Semantic => {
                    state.semantic_view_ids.remove(&self.view_id);
                }
            }
        }
    }
}

fn validate_correlation(
    state: &RuntimeState,
    correlation: RuntimeCorrelation,
) -> Result<(), ProviderSessionError> {
    if state.task_id != correlation.task_id {
        return Err(ProviderSessionError::WrongTask {
            expected: state.task_id,
            actual: correlation.task_id,
        });
    }
    if state.agent_session_id != correlation.agent_session_id {
        return Err(ProviderSessionError::WrongAgentSession {
            expected: state.agent_session_id,
            actual: correlation.agent_session_id,
        });
    }
    if state.provider_kind != correlation.provider_kind {
        return Err(ProviderSessionError::WrongProviderKind {
            expected: state.provider_kind,
            actual: correlation.provider_kind,
        });
    }
    if state.correlation.generation != correlation.generation {
        return Err(ProviderSessionError::StaleGeneration {
            expected: state.correlation.generation,
            actual: correlation.generation,
        });
    }
    if state.correlation.launch_nonce != correlation.launch_nonce {
        return Err(ProviderSessionError::WrongLaunchNonce);
    }
    Ok(())
}

/// Host-owned map of the current provider generation for each AgentSession.
pub struct ProviderSessionManager<L, S = InMemoryProviderSessionStateStore>
where
    L: ProviderProcessLauncher,
    S: ProviderSessionStateStore,
{
    launcher: L,
    state_store: S,
    current: HashMap<AgentSessionId, ProviderRuntime>,
    leases: HashMap<AgentSessionId, L::Lease>,
    next_generation: HashMap<AgentSessionId, u64>,
    next_state_revision: HashMap<AgentSessionId, u64>,
    next_view_id: u64,
}

impl<L: ProviderProcessLauncher> ProviderSessionManager<L, InMemoryProviderSessionStateStore> {
    /// Test/fixture constructor.  Production integration must use
    /// [`ProviderSessionManager::with_state_store`] with the durable Task
    /// store; this constructor does not claim crash recovery.
    pub fn new(launcher: L) -> Self {
        Self::with_state_store(launcher, InMemoryProviderSessionStateStore::default())
    }
}

impl<L: ProviderProcessLauncher, S: ProviderSessionStateStore> ProviderSessionManager<L, S> {
    pub fn with_state_store(launcher: L, state_store: S) -> Self {
        Self {
            launcher,
            state_store,
            current: HashMap::new(),
            leases: HashMap::new(),
            next_generation: HashMap::new(),
            next_state_revision: HashMap::new(),
            next_view_id: 0,
        }
    }

    pub fn current(&self, agent_session_id: AgentSessionId) -> Option<ProviderRuntime> {
        self.current.get(&agent_session_id).cloned()
    }

    pub fn start(
        &mut self,
        request: StartProviderSessionRequest,
    ) -> Result<ProviderRuntime, ProviderSessionError> {
        let mode = resolve_launch_mode(&request)?;
        self.ensure_request_admissible(&request)?;
        let agent_id = request.agent.id;
        if let Some(existing) = self.current.get(&agent_id) {
            match existing.lifecycle() {
                RuntimeLifecycle::Running | RuntimeLifecycle::Stopping => {
                    return Err(ProviderSessionError::SessionAlreadyRunning(agent_id))
                }
                RuntimeLifecycle::Closed => {
                    return Err(ProviderSessionError::SessionClosed(agent_id))
                }
                RuntimeLifecycle::Exited | RuntimeLifecycle::Replaced => {}
            }
        }

        let persisted = self.load_state(agent_id)?;
        if let Some(state) = &persisted {
            let revision = self.next_state_revision.entry(agent_id).or_insert(0);
            *revision = (*revision).max(state.revision());
            if state.task_id != request.agent.task_id {
                return Err(ProviderSessionError::WrongTask {
                    expected: state.task_id,
                    actual: request.agent.task_id,
                });
            }
            if state.lifecycle == PersistedRuntimeLifecycle::Closed {
                return Err(ProviderSessionError::SessionClosed(agent_id));
            }
            if request.agent.runtime_generation < state.generation {
                return Err(ProviderSessionError::StaleAgentFacts {
                    generation: state.generation,
                });
            }
            if matches!(
                state.lifecycle,
                PersistedRuntimeLifecycle::Starting
                    | PersistedRuntimeLifecycle::Running
                    | PersistedRuntimeLifecycle::Stopping
            ) && self.current.get(&agent_id).is_none()
            {
                return Err(ProviderSessionError::SettlementRequired {
                    agent_session_id: agent_id,
                    generation: state.generation,
                });
            }
        }
        let generation = self.allocate_generation(
            agent_id,
            request.agent.runtime_generation,
            persisted.as_ref().map(ProviderSessionState::generation),
        )?;
        let correlation = RuntimeCorrelation {
            task_id: request.agent.task_id,
            agent_session_id: agent_id,
            provider_kind: request.agent.provider_kind,
            generation,
            launch_nonce: LaunchNonce::new(),
        };
        let resource_id = ResourceId::new();
        let terminal_id = TerminalId::new();
        let launch_spec = ProviderLaunchSpec {
            provider_kind: request.agent.provider_kind,
            executable: request.executable.clone(),
            mode: mode.clone(),
            arguments: Vec::new(),
            cwd: std::env::current_dir()
                .map_err(|error| ProviderSessionError::StateStore(error.to_string()))?,
            environment: BTreeMap::new(),
            task_id: request.agent.task_id,
            resource_id,
            terminal_id,
            generation,
            launch_nonce: correlation.launch_nonce,
        };
        let launch_request = ProviderRuntimeLaunchRequest {
            correlation,
            launch_spec,
            capabilities: request.capabilities.clone(),
        };
        self.persist_state(
            &request.agent,
            generation,
            PersistedRuntimeLifecycle::Starting,
            correlation.launch_nonce,
        )?;
        let lease = self
            .launcher
            .launch(&launch_request)
            .map_err(|error| self.map_launch_error(&mode, error))?;
        if let Err(error) = validate_lease(&launch_request, &lease) {
            // The launcher may already have registered a Job/PTY tree even if
            // its returned identity is unusable. Never let a failed
            // validation drop that ownership token while the tree may live.
            std::mem::forget(lease);
            return Err(error);
        }
        self.leases.insert(agent_id, lease);
        self.persist_state(
            &request.agent,
            generation,
            PersistedRuntimeLifecycle::Running,
            correlation.launch_nonce,
        )?;
        let identity = match mode {
            ProviderLaunchMode::NewConversation => ProviderIdentityState::Pending,
            ProviderLaunchMode::ResumeExact(provider_session_id) => {
                ProviderIdentityState::Expected(provider_session_id)
            }
        };
        let runtime = ProviderRuntime::new(
            &launch_request,
            &request.agent,
            self.leases
                .get(&agent_id)
                .expect("lease inserted")
                .fence()
                .clone(),
            identity,
            terminal_id,
        );
        self.current.insert(agent_id, runtime.clone());
        Ok(runtime)
    }

    pub fn replace_generation(
        &mut self,
        request: StartProviderSessionRequest,
    ) -> Result<ProviderRuntime, ProviderSessionError> {
        let _ = resolve_launch_mode(&request)?;
        self.ensure_request_admissible(&request)?;
        let agent_id = request.agent.id;
        if let Some(existing) = self.current.get(&agent_id).cloned() {
            if existing.task_id() != request.agent.task_id {
                return Err(ProviderSessionError::WrongTask {
                    expected: existing.task_id(),
                    actual: request.agent.task_id,
                });
            }
            self.settle_runtime(agent_id, &existing, PersistedRuntimeLifecycle::Replaced)?;
        } else if let Some(state) = self.load_state(agent_id)? {
            if matches!(
                state.lifecycle,
                PersistedRuntimeLifecycle::Starting
                    | PersistedRuntimeLifecycle::Running
                    | PersistedRuntimeLifecycle::Stopping
            ) {
                return Err(ProviderSessionError::SettlementRequired {
                    agent_session_id: agent_id,
                    generation: state.generation,
                });
            }
        }
        self.start(request)
    }

    pub fn attach_terminal_view(
        &mut self,
        correlation: RuntimeCorrelation,
    ) -> Result<ProviderView, ProviderSessionError> {
        let runtime = self.live_runtime(correlation)?;
        let view_id = self.allocate_view_id();
        runtime.attach_terminal_view(view_id)
    }

    pub fn subscribe_semantic(
        &mut self,
        correlation: RuntimeCorrelation,
    ) -> Result<ProviderView, ProviderSessionError> {
        let runtime = self.live_runtime(correlation)?;
        let view_id = self.allocate_view_id();
        runtime.attach_semantic_view(view_id)
    }

    pub fn close_view(&mut self, view: &ProviderView) -> Result<(), ProviderSessionError> {
        let runtime = self.current.get(&view.agent_session_id()).cloned().ok_or(
            ProviderSessionError::AgentSessionNotFound(view.agent_session_id()),
        )?;
        runtime.validate_correlation(view.correlation())?;
        runtime.detach_view(view)
    }

    /// Raw provider IDs are observations, not identity authority.  The only
    /// accepted path is [`Self::accept_provider_session_start`] with a token
    /// minted by Task 4.1b's authenticated current-generation registry.
    pub fn accept_provider_session_id(
        &mut self,
        _correlation: RuntimeCorrelation,
        _provider_session_id: ProviderSessionId,
    ) -> Result<ProviderIdentityAcceptance, ProviderSessionError> {
        Err(ProviderSessionError::UntrustedSessionStart)
    }

    pub fn accept_provider_session_start(
        &mut self,
        provenance: ProviderSessionStartProvenance,
    ) -> Result<ProviderIdentityAcceptance, ProviderSessionError> {
        let runtime = self.runtime_for_correlation(provenance.correlation)?;
        if provenance.correlation.launch_nonce != runtime.launch_nonce() {
            return Err(ProviderSessionError::SessionStartProvenanceMismatch);
        }
        runtime.accept_provider_session_start(&provenance)
    }

    /// Root exit is observation only.  The Job/PTY lease remains authoritative
    /// until a joined ACTIVE_PROCESS_ZERO settlement permits replacement or
    /// close.
    pub fn process_exited(
        &mut self,
        correlation: RuntimeCorrelation,
    ) -> Result<(), ProviderSessionError> {
        let runtime = self.runtime_for_correlation(correlation)?;
        runtime.mark_root_exit_observed()
    }

    pub fn close_agent_session(
        &mut self,
        agent_session_id: AgentSessionId,
    ) -> Result<(), ProviderSessionError> {
        let Some(runtime) = self.current.get(&agent_session_id).cloned() else {
            return match self.load_state(agent_session_id)? {
                Some(state) if state.lifecycle == PersistedRuntimeLifecycle::Closed => Ok(()),
                Some(state)
                    if matches!(
                        state.lifecycle,
                        PersistedRuntimeLifecycle::Starting
                            | PersistedRuntimeLifecycle::Running
                            | PersistedRuntimeLifecycle::Stopping
                    ) =>
                {
                    Err(ProviderSessionError::SettlementRequired {
                        agent_session_id,
                        generation: state.generation,
                    })
                }
                _ => Err(ProviderSessionError::AgentSessionNotFound(agent_session_id)),
            };
        };
        if runtime.lifecycle() == RuntimeLifecycle::Closed {
            return Ok(());
        }
        self.settle_runtime(
            agent_session_id,
            &runtime,
            PersistedRuntimeLifecycle::Closed,
        )
    }

    pub fn close_task(&mut self, task_id: TaskId) -> Result<(), ProviderSessionError> {
        let agent_ids: Vec<_> = self
            .current
            .iter()
            .filter_map(|(agent_id, runtime)| (runtime.task_id() == task_id).then_some(*agent_id))
            .collect();
        for agent_id in agent_ids {
            self.close_agent_session(agent_id)?;
        }
        Ok(())
    }

    fn ensure_request_admissible(
        &self,
        request: &StartProviderSessionRequest,
    ) -> Result<(), ProviderSessionError> {
        if request.agent.lifecycle != crate::domain::AgentSessionLifecycle::Open {
            return Err(ProviderSessionError::SessionClosed(request.agent.id));
        }
        if request.agent.provider_kind != request.capabilities.kind {
            return Err(ProviderSessionError::ProviderKindMismatch {
                agent: request.agent.provider_kind,
                capabilities: request.capabilities.kind,
            });
        }
        request
            .capabilities
            .validate()
            .map_err(|_| ProviderSessionError::InvalidCapabilities)
    }

    fn load_state(
        &mut self,
        agent_session_id: AgentSessionId,
    ) -> Result<Option<ProviderSessionState>, ProviderSessionError> {
        let state = self
            .state_store
            .load(agent_session_id)
            .map_err(ProviderSessionError::StateStore)?;
        if let Some(state) = &state {
            if state.generation == 0 || state.revision == 0 {
                return Err(ProviderSessionError::StalePersistedState {
                    generation: state.generation,
                });
            }
        }
        Ok(state)
    }

    fn persist_state(
        &mut self,
        agent: &AgentSessionFacts,
        generation: u64,
        lifecycle: PersistedRuntimeLifecycle,
        launch_nonce: LaunchNonce,
    ) -> Result<(), ProviderSessionError> {
        let revision = self.next_state_revision.entry(agent.id).or_insert(0);
        *revision = revision
            .checked_add(1)
            .ok_or(ProviderSessionError::GenerationExhausted)?;
        self.state_store
            .persist(ProviderSessionState {
                agent_session_id: agent.id,
                task_id: agent.task_id,
                generation,
                revision: *revision,
                lifecycle,
                launch_nonce,
            })
            .map_err(ProviderSessionError::StateStore)
    }

    fn settle_runtime(
        &mut self,
        agent_id: AgentSessionId,
        runtime: &ProviderRuntime,
        final_state: PersistedRuntimeLifecycle,
    ) -> Result<(), ProviderSessionError> {
        match runtime.lifecycle() {
            RuntimeLifecycle::Running | RuntimeLifecycle::Stopping => {}
            lifecycle => return Err(ProviderSessionError::RuntimeNotLive { lifecycle }),
        }
        self.persist_state_for_runtime(runtime, PersistedRuntimeLifecycle::Stopping)?;
        runtime.mark_stopping()?;
        let lease =
            self.leases
                .get_mut(&agent_id)
                .ok_or(ProviderSessionError::SettlementRequired {
                    agent_session_id: agent_id,
                    generation: runtime.generation(),
                })?;
        let settlement = self
            .launcher
            .stop_and_join(lease)
            .map_err(ProviderSessionError::StopFailed)?;
        if settlement.fence() != &runtime.fence() || !settlement.is_joined_active_process_zero() {
            return Err(if settlement.fence() != &runtime.fence() {
                ProviderSessionError::SettlementFenceMismatch
            } else {
                ProviderSessionError::SettlementRequired {
                    agent_session_id: agent_id,
                    generation: runtime.generation(),
                }
            });
        }
        self.persist_state_for_runtime(runtime, final_state)?;
        match final_state {
            PersistedRuntimeLifecycle::Replaced => runtime.mark_replaced(),
            PersistedRuntimeLifecycle::Closed => runtime.mark_closed(),
            _ => unreachable!("settlement final state must be Replaced or Closed"),
        }
        self.leases.remove(&agent_id);
        Ok(())
    }

    fn persist_state_for_runtime(
        &mut self,
        runtime: &ProviderRuntime,
        lifecycle: PersistedRuntimeLifecycle,
    ) -> Result<(), ProviderSessionError> {
        let agent = AgentSessionFacts {
            id: runtime.agent_session_id(),
            task_id: runtime.task_id(),
            role: runtime.role(),
            provider_kind: runtime.provider_kind(),
            provider_session_id: runtime.provider_session_id(),
            lifecycle: crate::domain::AgentSessionLifecycle::Open,
            runtime_generation: runtime.generation(),
            revision: 0,
        };
        self.persist_state(
            &agent,
            runtime.generation(),
            lifecycle,
            runtime.launch_nonce(),
        )
    }

    fn map_launch_error(
        &self,
        mode: &ProviderLaunchMode,
        error: ProviderLaunchError,
    ) -> ProviderSessionError {
        if let ProviderLaunchMode::ResumeExact(provider_session_id) = mode {
            if let ProviderLaunchError::ExactResumeFailed(failure) = error {
                return ProviderSessionError::ExactResumeFailed {
                    provider_session_id: provider_session_id.clone(),
                    failure,
                };
            }
        }
        ProviderSessionError::LaunchFailed(error)
    }

    fn runtime_for_correlation(
        &self,
        correlation: RuntimeCorrelation,
    ) -> Result<ProviderRuntime, ProviderSessionError> {
        let runtime = self
            .current
            .get(&correlation.agent_session_id())
            .cloned()
            .ok_or(ProviderSessionError::AgentSessionNotFound(
                correlation.agent_session_id(),
            ))?;
        runtime.validate_correlation(correlation)?;
        Ok(runtime)
    }

    fn live_runtime(
        &self,
        correlation: RuntimeCorrelation,
    ) -> Result<ProviderRuntime, ProviderSessionError> {
        let runtime = self.runtime_for_correlation(correlation)?;
        if runtime.lifecycle() != RuntimeLifecycle::Running {
            return Err(ProviderSessionError::RuntimeNotLive {
                lifecycle: runtime.lifecycle(),
            });
        }
        Ok(runtime)
    }

    fn allocate_generation(
        &mut self,
        agent_session_id: AgentSessionId,
        persisted_generation: u64,
        store_generation: Option<u64>,
    ) -> Result<u64, ProviderSessionError> {
        let baseline = persisted_generation.max(store_generation.unwrap_or(0));
        let next = match self.next_generation.get(&agent_session_id).copied() {
            Some(u64::MAX) => return Err(ProviderSessionError::GenerationExhausted),
            Some(next) if next > baseline => next,
            _ => baseline
                .checked_add(1)
                .ok_or(ProviderSessionError::GenerationExhausted)?,
        };
        self.next_generation
            .insert(agent_session_id, next.saturating_add(1));
        Ok(next)
    }

    fn allocate_view_id(&mut self) -> u64 {
        self.next_view_id = self.next_view_id.checked_add(1).unwrap_or(1);
        self.next_view_id
    }
}

impl<L: ProviderProcessLauncher, S: ProviderSessionStateStore> Drop
    for ProviderSessionManager<L, S>
{
    fn drop(&mut self) {
        // Drop is deliberately fail-closed: a manager that cannot run the
        // typed stop/join protocol must leak the lease rather than release a
        // Job/PTY owner while its tree may still exist.
        for (_, lease) in self.leases.drain() {
            std::mem::forget(lease);
        }
    }
}

fn validate_lease<L: ProviderProcessLease>(
    request: &ProviderRuntimeLaunchRequest,
    lease: &L,
) -> Result<(), ProviderSessionError> {
    let fence = lease.fence();
    if fence.resource()
        != ResourceFence::new(
            request.launch_spec.resource_id,
            request.launch_spec.generation,
        )
        || fence.owner() != ProcessOwner::Task(request.launch_spec.task_id)
        || fence.root().id().pid() == 0
        || fence.root().id().creation_time_100ns() == 0
        || fence.root().canonical_executable() != request.launch_spec.executable.canonical_path()
    {
        return Err(ProviderSessionError::LaunchFailed(
            ProviderLaunchError::ProcessFenceMismatch,
        ));
    }
    Ok(())
}

fn resolve_launch_mode(
    request: &StartProviderSessionRequest,
) -> Result<ProviderLaunchMode, ProviderSessionError> {
    if request.agent.provider_kind != request.capabilities.kind {
        return Err(ProviderSessionError::ProviderKindMismatch {
            agent: request.agent.provider_kind,
            capabilities: request.capabilities.kind,
        });
    }
    match request.mode {
        ProviderSessionStartMode::NewConversation => Ok(ProviderLaunchMode::NewConversation),
        ProviderSessionStartMode::Open | ProviderSessionStartMode::ResumeExact => {
            let Some(provider_session_id) = request.agent.provider_session_id.clone() else {
                return Err(match request.mode {
                    ProviderSessionStartMode::Open => {
                        ProviderSessionError::ExplicitNewConversationRequired {
                            agent_session_id: request.agent.id,
                        }
                    }
                    ProviderSessionStartMode::ResumeExact => {
                        ProviderSessionError::CorrelatedProviderSessionRequired {
                            agent_session_id: request.agent.id,
                        }
                    }
                    ProviderSessionStartMode::NewConversation => unreachable!(),
                });
            };
            Ok(ProviderLaunchMode::ResumeExact(provider_session_id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::identity::ManagedProcessIdentity;

    #[test]
    fn wrong_nonce_fixture_setter_is_test_only_and_cannot_bind() {
        let correlation = RuntimeCorrelation {
            task_id: TaskId::new(),
            agent_session_id: AgentSessionId::new(),
            provider_kind: ProviderKind::ClaudeCode,
            generation: 1,
            launch_nonce: LaunchNonce::new(),
        };
        assert_ne!(
            correlation.launch_nonce(),
            correlation
                .set_launch_nonce_for_test(LaunchNonce::new())
                .launch_nonce()
        );
    }

    #[test]
    fn authenticated_provenance_is_minted_only_inside_the_crate_boundary() {
        let correlation = RuntimeCorrelation {
            task_id: TaskId::new(),
            agent_session_id: AgentSessionId::new(),
            provider_kind: ProviderKind::ClaudeCode,
            generation: 1,
            launch_nonce: LaunchNonce::new(),
        };
        let id = ProviderSessionId::new("session").unwrap();
        let provenance = ProviderSessionStartProvenance::from_authenticated_current_generation(
            correlation,
            id.clone(),
        );
        assert_eq!(provenance.correlation(), correlation);
        assert_eq!(provenance.provider_session_id(), &id);
    }

    fn unit_runtime() -> ProviderRuntime {
        let task_id = TaskId::new();
        let agent =
            AgentSessionFacts::new(task_id, AgentRole::Primary, ProviderKind::ClaudeCode, None)
                .unwrap();
        let executable = ProviderExecutable::from_path(std::env::current_exe().unwrap()).unwrap();
        let correlation = RuntimeCorrelation {
            task_id,
            agent_session_id: agent.id,
            provider_kind: ProviderKind::ClaudeCode,
            generation: 1,
            launch_nonce: LaunchNonce::new(),
        };
        let launch_spec = ProviderLaunchSpec {
            provider_kind: ProviderKind::ClaudeCode,
            executable: executable.clone(),
            mode: ProviderLaunchMode::NewConversation,
            arguments: Vec::new(),
            cwd: std::env::current_dir().unwrap(),
            environment: BTreeMap::new(),
            task_id,
            resource_id: ResourceId::new(),
            terminal_id: TerminalId::new(),
            generation: 1,
            launch_nonce: correlation.launch_nonce,
        };
        let request = ProviderRuntimeLaunchRequest {
            correlation,
            launch_spec,
            capabilities: ProviderCapabilities {
                kind: ProviderKind::ClaudeCode,
                version: crate::providers::capabilities::ProviderVersion::new("fixture").unwrap(),
                auth_state: crate::providers::capabilities::ProviderAuthState::Unknown,
                exact_resume: crate::providers::capabilities::CapabilitySupport::Supported,
                semantic_events: crate::providers::capabilities::CapabilitySupport::Supported,
                provider_session_id: crate::providers::capabilities::CapabilitySupport::Supported,
                build_launch: crate::providers::capabilities::CapabilitySupport::Supported,
                parse_signal: crate::providers::capabilities::CapabilitySupport::Supported,
                cooperative_stop: crate::providers::capabilities::CapabilitySupport::Supported,
                observe_quota: crate::providers::capabilities::CapabilitySupport::Unknown,
                evidence: Vec::new(),
            },
        };
        let process_id = ManagedProcessId::new(1, 1).unwrap();
        let root = ManagedProcessIdentity::new(process_id, executable.canonical_path()).unwrap();
        let fence = ManagedProcessFence::new(
            ResourceFence::new(
                request.launch_spec.resource_id,
                request.launch_spec.generation,
            ),
            ProcessOwner::Task(task_id),
            root,
        );
        ProviderRuntime::new(
            &request,
            &agent,
            fence,
            ProviderIdentityState::Pending,
            request.launch_spec.terminal_id,
        )
    }

    #[test]
    fn session_start_accepts_only_first_authenticated_current_generation() {
        let runtime = unit_runtime();
        let correlation = runtime.correlation();
        let first_id = ProviderSessionId::new("provider-session-1").unwrap();
        let second_id = ProviderSessionId::new("provider-session-2").unwrap();
        let first = ProviderSessionStartProvenance::from_authenticated_current_generation(
            correlation,
            first_id.clone(),
        );
        assert_eq!(
            runtime.accept_provider_session_start(&first).unwrap(),
            ProviderIdentityAcceptance::Accepted
        );
        assert_eq!(
            runtime.accept_provider_session_start(&first).unwrap(),
            ProviderIdentityAcceptance::AlreadyAccepted
        );
        let rebind = ProviderSessionStartProvenance::from_authenticated_current_generation(
            correlation,
            second_id,
        );
        assert!(matches!(
            runtime.accept_provider_session_start(&rebind),
            Err(ProviderSessionError::ProviderSessionIdRebind { .. })
        ));
        assert_eq!(runtime.provider_session_id(), Some(first_id));
    }

    #[test]
    fn session_start_rejects_wrong_nonce_generation_and_untrusted_provenance() {
        let runtime = unit_runtime();
        let id = ProviderSessionId::new("provider-session").unwrap();
        let wrong_nonce = ProviderSessionStartProvenance::from_authenticated_current_generation(
            runtime
                .correlation()
                .set_launch_nonce_for_test(LaunchNonce::new()),
            id.clone(),
        );
        assert!(matches!(
            runtime.accept_provider_session_start(&wrong_nonce),
            Err(ProviderSessionError::WrongLaunchNonce)
        ));
        let stale = ProviderSessionStartProvenance::from_authenticated_current_generation(
            RuntimeCorrelation {
                generation: runtime.generation() + 1,
                ..runtime.correlation()
            },
            id.clone(),
        );
        assert!(matches!(
            runtime.accept_provider_session_start(&stale),
            Err(ProviderSessionError::StaleGeneration { .. })
        ));
        let mut unauthenticated =
            ProviderSessionStartProvenance::from_authenticated_current_generation(
                runtime.correlation(),
                id,
            );
        unauthenticated.authenticated = false;
        assert!(matches!(
            runtime.accept_provider_session_start(&unauthenticated),
            Err(ProviderSessionError::UntrustedSessionStart)
        ));
    }
}
