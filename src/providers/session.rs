//! Provider runtime generations and presentation views.
//!
//! This module deliberately owns only the provider-session contract.  The
//! launcher is an injected boundary, so tests can prove lifecycle and
//! correlation rules without starting a provider executable.  The production
//! process/PTY services can implement [`ProviderProcessLauncher`] in a later
//! integration task.

use crate::domain::{
    AgentRole, AgentSessionFacts, AgentSessionId, ProviderSessionId, ResourceId, TaskId, TerminalId,
};
use crate::providers::capabilities::{ProviderCapabilities, ProviderExecutable, ProviderKind};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

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

/// Concrete process launch mode emitted after resolving a start request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderLaunchMode {
    NewConversation,
    ResumeExact(ProviderSessionId),
}

/// Request passed to the injected process launcher.  It is intentionally
/// provider-neutral while retaining the executable and capability snapshot
/// pinned to this generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRuntimeLaunchRequest {
    correlation: RuntimeCorrelation,
    provider_kind: ProviderKind,
    executable: ProviderExecutable,
    capabilities: ProviderCapabilities,
    mode: ProviderLaunchMode,
    resource_id: ResourceId,
    terminal_id: TerminalId,
}

impl ProviderRuntimeLaunchRequest {
    pub fn correlation(&self) -> RuntimeCorrelation {
        self.correlation
    }

    pub const fn provider_kind(&self) -> ProviderKind {
        self.provider_kind
    }

    pub fn executable(&self) -> &ProviderExecutable {
        &self.executable
    }

    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    pub fn mode(&self) -> &ProviderLaunchMode {
        &self.mode
    }

    pub const fn resource_id(&self) -> ResourceId {
        self.resource_id
    }

    pub const fn terminal_id(&self) -> TerminalId {
        self.terminal_id
    }
}

/// Opaque process identity returned by the injected launcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProviderProcessId(u64);

impl ProviderProcessId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Process handle owned by one provider runtime generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderProcess {
    id: ProviderProcessId,
}

impl ProviderProcess {
    pub const fn new(id: ProviderProcessId) -> Self {
        Self { id }
    }

    pub const fn id(self) -> ProviderProcessId {
        self.id
    }
}

/// Launch/teardown failures stay typed so exact-resume failures remain visible
/// to callers and cannot be silently retried as a fresh conversation.
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
}

/// The process/PTY integration implements this trait.  No implementation in
/// this module invokes a stock CLI; fake implementations are sufficient for
/// all session contract tests.
pub trait ProviderProcessLauncher {
    fn launch(
        &mut self,
        request: &ProviderRuntimeLaunchRequest,
    ) -> Result<ProviderProcess, ProviderLaunchError>;

    fn stop(&mut self, process: &ProviderProcess) -> Result<(), ProviderLaunchError>;
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
    Exited,
    Replaced,
    Closing,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderView {
    correlation: RuntimeCorrelation,
    process_id: ProviderProcessId,
    kind: ProviderViewKind,
    view_id: u64,
}

impl ProviderView {
    pub const fn correlation(self) -> RuntimeCorrelation {
        self.correlation
    }

    pub const fn process_id(self) -> ProviderProcessId {
        self.process_id
    }

    pub const fn kind(self) -> ProviderViewKind {
        self.kind
    }

    pub const fn agent_session_id(self) -> AgentSessionId {
        self.correlation.agent_session_id()
    }

    pub const fn task_id(self) -> TaskId {
        self.correlation.task_id()
    }

    pub const fn provider_kind(self) -> ProviderKind {
        self.correlation.provider_kind()
    }

    pub const fn generation(self) -> u64 {
        self.correlation.generation()
    }

    pub const fn launch_nonce(self) -> LaunchNonce {
        self.correlation.launch_nonce()
    }
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
    GenerationExhausted,
    LaunchFailed(ProviderLaunchError),
    ExactResumeFailed {
        provider_session_id: ProviderSessionId,
        failure: ExactResumeFailure,
    },
    StopFailed(ProviderLaunchError),
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
    ProviderSessionIdRebind {
        existing: ProviderSessionId,
        received: ProviderSessionId,
    },
    TerminalViewAlreadyAttached,
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
            Self::GenerationExhausted => {
                formatter.write_str("provider runtime generation exhausted")
            }
            Self::LaunchFailed(error) => write!(formatter, "provider launch failed: {error:?}"),
            Self::ExactResumeFailed { failure, .. } => {
                write!(formatter, "exact provider resume failed: {failure:?}")
            }
            Self::StopFailed(error) => write!(formatter, "provider process stop failed: {error:?}"),
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
            Self::ProviderSessionIdRebind { .. } => {
                formatter.write_str("provider session ID attempted to rebind")
            }
            Self::TerminalViewAlreadyAttached => {
                formatter.write_str("raw terminal view is already attached")
            }
            Self::ViewNotAttached => formatter.write_str("provider view is not attached"),
            Self::ViewFromDifferentRuntime => {
                formatter.write_str("provider view belongs to another runtime")
            }
        }
    }
}

impl std::error::Error for ProviderSessionError {}

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
    process_id: ProviderProcessId,
    lifecycle: RuntimeLifecycle,
    identity: ProviderIdentityState,
    terminal_view_id: Option<u64>,
    semantic_view_ids: HashSet<u64>,
}

/// Shared immutable runtime identity with synchronized lifecycle/identity
/// state.  Clones refer to the same provider process generation.
#[derive(Clone, Debug)]
pub struct ProviderRuntime {
    state: Arc<Mutex<RuntimeState>>,
}

impl ProviderRuntime {
    fn new(
        request: &ProviderRuntimeLaunchRequest,
        agent: &AgentSessionFacts,
        process: ProviderProcess,
        identity: ProviderIdentityState,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(RuntimeState {
                correlation: request.correlation,
                task_id: agent.task_id,
                agent_session_id: agent.id,
                role: agent.role.clone(),
                provider_kind: request.provider_kind,
                resource_id: request.resource_id,
                terminal_id: request.terminal_id,
                executable: request.executable.clone(),
                capabilities: request.capabilities.clone(),
                process_id: process.id,
                lifecycle: RuntimeLifecycle::Running,
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

    pub fn process_id(&self) -> ProviderProcessId {
        self.state.lock().unwrap().process_id
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

    pub fn accept_provider_session_id(
        &self,
        correlation: RuntimeCorrelation,
        provider_session_id: ProviderSessionId,
    ) -> Result<ProviderIdentityAcceptance, ProviderSessionError> {
        let mut state = self.state.lock().unwrap();
        validate_correlation(&state, correlation)?;
        if state.lifecycle != RuntimeLifecycle::Running {
            return Err(ProviderSessionError::RuntimeNotLive {
                lifecycle: state.lifecycle,
            });
        }
        match &state.identity {
            ProviderIdentityState::Pending => {
                state.identity = ProviderIdentityState::Accepted(provider_session_id);
                Ok(ProviderIdentityAcceptance::Accepted)
            }
            ProviderIdentityState::Expected(expected) if expected == &provider_session_id => {
                state.identity = ProviderIdentityState::Accepted(provider_session_id);
                Ok(ProviderIdentityAcceptance::Accepted)
            }
            ProviderIdentityState::Expected(expected) => {
                Err(ProviderSessionError::ProviderSessionIdRebind {
                    existing: expected.clone(),
                    received: provider_session_id,
                })
            }
            ProviderIdentityState::Accepted(existing) if existing == &provider_session_id => {
                Ok(ProviderIdentityAcceptance::AlreadyAccepted)
            }
            ProviderIdentityState::Accepted(existing) => {
                Err(ProviderSessionError::ProviderSessionIdRebind {
                    existing: existing.clone(),
                    received: provider_session_id,
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
        Ok(ProviderView {
            correlation: state.correlation,
            process_id: state.process_id,
            kind: ProviderViewKind::RawTerminal,
            view_id,
        })
    }

    fn attach_semantic_view(&self, view_id: u64) -> Result<ProviderView, ProviderSessionError> {
        let mut state = self.state.lock().unwrap();
        if state.lifecycle != RuntimeLifecycle::Running {
            return Err(ProviderSessionError::RuntimeNotLive {
                lifecycle: state.lifecycle,
            });
        }
        state.semantic_view_ids.insert(view_id);
        Ok(ProviderView {
            correlation: state.correlation,
            process_id: state.process_id,
            kind: ProviderViewKind::Semantic,
            view_id,
        })
    }

    fn close_view(&self, view: &ProviderView) -> Result<(), ProviderSessionError> {
        let mut state = self.state.lock().unwrap();
        if state.correlation != view.correlation || state.process_id != view.process_id {
            return Err(ProviderSessionError::ViewFromDifferentRuntime);
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

    fn mark_exited(&self) -> Result<(), ProviderSessionError> {
        let mut state = self.state.lock().unwrap();
        if state.lifecycle != RuntimeLifecycle::Running {
            return Err(ProviderSessionError::RuntimeNotLive {
                lifecycle: state.lifecycle,
            });
        }
        state.lifecycle = RuntimeLifecycle::Exited;
        Ok(())
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
/// Presentation handles never own process lifetime; task/session close and
/// process exit are the only paths that change runtime ownership here.
pub struct ProviderSessionManager<L> {
    launcher: L,
    current: HashMap<AgentSessionId, ProviderRuntime>,
    next_generation: HashMap<AgentSessionId, u64>,
    next_view_id: u64,
}

impl<L: ProviderProcessLauncher> ProviderSessionManager<L> {
    pub fn new(launcher: L) -> Self {
        Self {
            launcher,
            current: HashMap::new(),
            next_generation: HashMap::new(),
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
        let agent_id = request.agent.id;
        if let Some(existing) = self.current.get(&agent_id) {
            match existing.lifecycle() {
                RuntimeLifecycle::Running | RuntimeLifecycle::Closing => {
                    return Err(ProviderSessionError::SessionAlreadyRunning(agent_id));
                }
                RuntimeLifecycle::Closed => {
                    return Err(ProviderSessionError::SessionClosed(agent_id));
                }
                RuntimeLifecycle::Exited | RuntimeLifecycle::Replaced => {}
            }
        }

        let generation = self.allocate_generation(agent_id, request.agent.runtime_generation)?;
        let correlation = RuntimeCorrelation {
            task_id: request.agent.task_id,
            agent_session_id: agent_id,
            provider_kind: request.agent.provider_kind,
            generation,
            launch_nonce: LaunchNonce::new(),
        };
        let resource_id = ResourceId::new();
        let terminal_id = TerminalId::new();
        let launch_request = ProviderRuntimeLaunchRequest {
            correlation,
            provider_kind: request.agent.provider_kind,
            executable: request.executable,
            capabilities: request.capabilities,
            mode: mode.clone(),
            resource_id,
            terminal_id,
        };
        let process = self.launcher.launch(&launch_request).map_err(|error| {
            if let ProviderLaunchMode::ResumeExact(provider_session_id) = &mode {
                if let ProviderLaunchError::ExactResumeFailed(failure) = error {
                    return ProviderSessionError::ExactResumeFailed {
                        provider_session_id: provider_session_id.clone(),
                        failure,
                    };
                }
            }
            ProviderSessionError::LaunchFailed(error)
        })?;
        let identity = match mode {
            ProviderLaunchMode::NewConversation => ProviderIdentityState::Pending,
            ProviderLaunchMode::ResumeExact(provider_session_id) => {
                ProviderIdentityState::Expected(provider_session_id)
            }
        };
        let runtime = ProviderRuntime::new(&launch_request, &request.agent, process, identity);
        self.current.insert(agent_id, runtime.clone());
        Ok(runtime)
    }

    pub fn replace_generation(
        &mut self,
        request: StartProviderSessionRequest,
    ) -> Result<ProviderRuntime, ProviderSessionError> {
        let _ = resolve_launch_mode(&request)?;
        let agent_id = request.agent.id;
        if let Some(existing) = self.current.get(&agent_id).cloned() {
            if existing.task_id() != request.agent.task_id {
                return Err(ProviderSessionError::WrongTask {
                    expected: existing.task_id(),
                    actual: request.agent.task_id,
                });
            }
            match existing.lifecycle() {
                RuntimeLifecycle::Running => {
                    self.launcher
                        .stop(&ProviderProcess::new(existing.process_id()))
                        .map_err(ProviderSessionError::StopFailed)?;
                    existing.mark_replaced();
                }
                RuntimeLifecycle::Exited | RuntimeLifecycle::Replaced => {
                    existing.mark_replaced();
                }
                RuntimeLifecycle::Closing | RuntimeLifecycle::Closed => {
                    return Err(ProviderSessionError::SessionClosed(agent_id));
                }
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
        runtime.close_view(view)
    }

    pub fn accept_provider_session_id(
        &mut self,
        correlation: RuntimeCorrelation,
        provider_session_id: ProviderSessionId,
    ) -> Result<ProviderIdentityAcceptance, ProviderSessionError> {
        let runtime = self.runtime_for_correlation(correlation)?;
        runtime.accept_provider_session_id(correlation, provider_session_id)
    }

    pub fn process_exited(
        &mut self,
        correlation: RuntimeCorrelation,
    ) -> Result<(), ProviderSessionError> {
        let runtime = self.runtime_for_correlation(correlation)?;
        runtime.mark_exited()
    }

    pub fn close_agent_session(
        &mut self,
        agent_session_id: AgentSessionId,
    ) -> Result<(), ProviderSessionError> {
        let Some(runtime) = self.current.get(&agent_session_id).cloned() else {
            return Err(ProviderSessionError::AgentSessionNotFound(agent_session_id));
        };
        match runtime.lifecycle() {
            RuntimeLifecycle::Running => {
                self.launcher
                    .stop(&ProviderProcess::new(runtime.process_id()))
                    .map_err(ProviderSessionError::StopFailed)?;
            }
            RuntimeLifecycle::Exited | RuntimeLifecycle::Replaced | RuntimeLifecycle::Closing => {}
            RuntimeLifecycle::Closed => return Ok(()),
        }
        runtime.mark_closed();
        Ok(())
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
    ) -> Result<u64, ProviderSessionError> {
        let next = match self.next_generation.get(&agent_session_id).copied() {
            Some(u64::MAX) => return Err(ProviderSessionError::GenerationExhausted),
            Some(next) => next,
            None => persisted_generation
                .checked_add(1)
                .ok_or(ProviderSessionError::GenerationExhausted)?,
        };
        self.next_generation
            .insert(agent_session_id, next.saturating_add(1));
        Ok(next)
    }

    fn allocate_view_id(&mut self) -> u64 {
        self.next_view_id = self.next_view_id.saturating_add(1);
        self.next_view_id
    }
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
