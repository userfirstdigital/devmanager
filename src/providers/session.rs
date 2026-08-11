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
#[cfg(test)]
use crate::process::registry::ProviderPermitOwnership;
pub use crate::process::registry::{JoinedActiveProcessZeroProof, ProviderManagedProcessPermit};
use crate::providers::capabilities::{
    CapabilitySupport, ProviderCapabilities, ProviderExecutable, ProviderKind,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
pub const MAX_PROVIDER_LAUNCH_ARGUMENT_BYTES: usize = 2048;
pub const MAX_PROVIDER_LAUNCH_ENVIRONMENT_BYTES: usize = 32 * 1024;
pub const MAX_PROVIDER_LAUNCH_CWD_BYTES: usize = 32 * 1024;

/// Opaque correlation material issued once for every provider process
/// generation.  It is never derived from cwd, timestamps, transcript names,
/// or provider output.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct LaunchNonce(Uuid);

impl LaunchNonce {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub(crate) const fn raw(self) -> Uuid {
        self.0
    }

    fn from_raw(raw: Uuid) -> Self {
        Self(raw)
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

/// The exact adapter-owned launch material consumed by one provider runtime.
///
/// The adapter supplies all executable arguments, environment, working
/// directory, capability snapshot, and resume intent before the session
/// manager allocates a generation. The manager never fills in a missing
/// command with a shell, the current process directory, or an inferred
/// provider flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAdapterLaunchSpec {
    executable: ProviderExecutable,
    arguments: Vec<OsString>,
    cwd: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    capabilities: ProviderCapabilities,
}

impl ProviderAdapterLaunchSpec {
    fn bounded(
        executable: ProviderExecutable,
        arguments: Vec<OsString>,
        cwd: PathBuf,
        environment: BTreeMap<OsString, OsString>,
        capabilities: ProviderCapabilities,
    ) -> Result<Self, ProviderLaunchSpecError> {
        if arguments.len() > MAX_PROVIDER_LAUNCH_ARGUMENTS {
            return Err(ProviderLaunchSpecError::TooManyArguments);
        }
        if arguments
            .iter()
            .any(|argument| argument.len() > MAX_PROVIDER_LAUNCH_ARGUMENT_BYTES)
        {
            return Err(ProviderLaunchSpecError::ArgumentTooLong);
        }
        if environment.len() > MAX_PROVIDER_LAUNCH_ENVIRONMENT {
            return Err(ProviderLaunchSpecError::TooManyEnvironmentEntries);
        }
        if environment.iter().any(|(key, value)| {
            key.len() > MAX_PROVIDER_LAUNCH_ENVIRONMENT_BYTES
                || value.len() > MAX_PROVIDER_LAUNCH_ENVIRONMENT_BYTES
        }) {
            return Err(ProviderLaunchSpecError::EnvironmentEntryTooLong);
        }
        if cwd.as_os_str().is_empty() || cwd.as_os_str().len() > MAX_PROVIDER_LAUNCH_CWD_BYTES {
            return Err(ProviderLaunchSpecError::WorkingDirectoryUnavailable);
        }
        capabilities
            .validate()
            .map_err(|_| ProviderLaunchSpecError::InvalidCapabilities)?;
        Ok(Self {
            executable,
            arguments,
            cwd,
            environment,
            capabilities,
        })
    }

    /// Registry/Task 3 adapter handoff. The provider registry supplies the
    /// bounded executable graph and exact launch material; callers cannot
    /// construct this value from raw args or environment.
    #[allow(dead_code)]
    pub(crate) fn from_registry(
        executable: ProviderExecutable,
        arguments: Vec<OsString>,
        cwd: PathBuf,
        environment: BTreeMap<OsString, OsString>,
        capabilities: ProviderCapabilities,
    ) -> Result<Self, ProviderLaunchSpecError> {
        Self::bounded(executable, arguments, cwd, environment, capabilities)
    }

    #[cfg(test)]
    pub(crate) fn from_test(
        executable: ProviderExecutable,
        arguments: Vec<OsString>,
        cwd: PathBuf,
        environment: BTreeMap<OsString, OsString>,
        capabilities: ProviderCapabilities,
    ) -> Result<Self, ProviderLaunchSpecError> {
        Self::bounded(executable, arguments, cwd, environment, capabilities)
    }

    fn unavailable_placeholder(
        executable: ProviderExecutable,
        capabilities: ProviderCapabilities,
    ) -> Self {
        Self {
            executable,
            arguments: Vec::new(),
            cwd: PathBuf::new(),
            environment: BTreeMap::new(),
            capabilities,
        }
    }

    pub fn executable(&self) -> &ProviderExecutable {
        &self.executable
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

    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }
}

/// Opaque provider-registry proof for one bounded adapter launch. The digest
/// covers the executable identity, graph arguments/environment/cwd, capability
/// snapshot, and resume intent. It is not constructible by an external caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAdapterLaunchProof {
    spec: ProviderAdapterLaunchSpec,
    mode: ProviderSessionStartMode,
    provider_session_id: Option<ProviderSessionId>,
    digest: [u8; 32],
}

#[derive(Serialize)]
struct AdapterLaunchProofDigestWire<'a> {
    executable_path: &'a Path,
    executable_sha256: &'a [u8; 32],
    arguments: &'a [OsString],
    cwd: &'a Path,
    environment: &'a BTreeMap<OsString, OsString>,
    capabilities: &'a ProviderCapabilities,
    mode: u8,
    provider_session_id: Option<&'a ProviderSessionId>,
}

impl ProviderAdapterLaunchProof {
    #[allow(dead_code)]
    fn issue(
        spec: ProviderAdapterLaunchSpec,
        mode: ProviderSessionStartMode,
        provider_session_id: Option<ProviderSessionId>,
    ) -> Result<Self, ProviderLaunchSpecError> {
        match mode {
            ProviderSessionStartMode::NewConversation if provider_session_id.is_some() => {
                return Err(ProviderLaunchSpecError::ResumeIntentMismatch)
            }
            ProviderSessionStartMode::ResumeExact if provider_session_id.is_none() => {
                return Err(ProviderLaunchSpecError::ResumeIntentMismatch)
            }
            _ => {}
        }
        let digest = launch_proof_digest(&spec, mode, provider_session_id.as_ref());
        Ok(Self {
            spec,
            mode,
            provider_session_id,
            digest,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn from_registry(
        spec: ProviderAdapterLaunchSpec,
        mode: ProviderSessionStartMode,
        provider_session_id: Option<ProviderSessionId>,
    ) -> Result<Self, ProviderLaunchSpecError> {
        Self::issue(spec, mode, provider_session_id)
    }

    #[cfg(test)]
    pub(crate) fn from_test(
        spec: ProviderAdapterLaunchSpec,
        mode: ProviderSessionStartMode,
        provider_session_id: Option<ProviderSessionId>,
    ) -> Result<Self, ProviderLaunchSpecError> {
        Self::issue(spec, mode, provider_session_id)
    }

    fn matches_request(
        &self,
        mode: ProviderSessionStartMode,
        requested_provider_session_id: Option<&ProviderSessionId>,
    ) -> bool {
        if self.mode != mode || !self.has_valid_digest() {
            return false;
        }
        match self.mode {
            ProviderSessionStartMode::NewConversation => {
                self.provider_session_id.is_none() && requested_provider_session_id.is_none()
            }
            ProviderSessionStartMode::ResumeExact => {
                self.provider_session_id.as_ref() == requested_provider_session_id
            }
            ProviderSessionStartMode::Open => {
                requested_provider_session_id.is_none()
                    || self.provider_session_id.as_ref() == requested_provider_session_id
            }
        }
    }

    fn has_valid_digest(&self) -> bool {
        self.digest == launch_proof_digest(&self.spec, self.mode, self.provider_session_id.as_ref())
    }

    fn matches_resolved_mode(&self, resolved_mode: &ProviderLaunchMode) -> bool {
        if !self.has_valid_digest() {
            return false;
        }
        match (self.mode, resolved_mode) {
            (ProviderSessionStartMode::NewConversation, ProviderLaunchMode::NewConversation) => {
                self.provider_session_id.is_none()
            }
            (
                ProviderSessionStartMode::ResumeExact,
                ProviderLaunchMode::ResumeExact(provider_session_id),
            )
            | (
                ProviderSessionStartMode::Open,
                ProviderLaunchMode::ResumeExact(provider_session_id),
            ) => self.provider_session_id.as_ref() == Some(provider_session_id),
            _ => false,
        }
    }
}

fn launch_proof_digest(
    spec: &ProviderAdapterLaunchSpec,
    mode: ProviderSessionStartMode,
    provider_session_id: Option<&ProviderSessionId>,
) -> [u8; 32] {
    let wire = AdapterLaunchProofDigestWire {
        executable_path: spec.executable.canonical_path(),
        executable_sha256: spec.executable.sha256(),
        arguments: &spec.arguments,
        cwd: &spec.cwd,
        environment: &spec.environment,
        capabilities: &spec.capabilities,
        mode: match mode {
            ProviderSessionStartMode::Open => 0,
            ProviderSessionStartMode::NewConversation => 1,
            ProviderSessionStartMode::ResumeExact => 2,
        },
        provider_session_id,
    };
    let encoded = serde_json::to_vec(&wire).expect("bounded launch proof is serializable");
    let mut hasher = Sha256::new();
    hasher.update(encoded);
    hasher.finalize().into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderLaunchSpecError {
    TooManyArguments,
    ArgumentTooLong,
    TooManyEnvironmentEntries,
    EnvironmentEntryTooLong,
    InvalidCapabilities,
    WorkingDirectoryUnavailable,
    ResumeIntentMismatch,
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
    capabilities: ProviderCapabilities,
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

    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
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
}

impl ProviderRuntimeLaunchRequest {
    pub fn correlation(&self) -> RuntimeCorrelation {
        self.correlation
    }

    pub fn launch_spec(&self) -> &ProviderLaunchSpec {
        &self.launch_spec
    }

    pub fn capabilities(&self) -> &ProviderCapabilities {
        self.launch_spec.capabilities()
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

/// Compatibility names are retained only for the current in-tree launcher
/// contract. Their constructors are not exposed: the real authority types are
/// the registry-issued, non-Clone capabilities below.
pub type ProviderProcessLease = ProviderManagedProcessPermit;
pub type ActiveProcessZeroSettlement = JoinedActiveProcessZeroProof;

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
    BridgeUnavailable,
}

/// Explicit integration seam for Task 3.4/4.1b. The public trait is sealed so
/// callers cannot provide a fake lease or zero proof by implementing it. The
/// production implementation remains [`UnavailableProviderProcessLauncher`]
/// until the Task 3 suspended Job-root/PTY bridge is joined.
pub trait ProviderProcessLauncher: sealed::ProviderProcessLauncher {
    fn launch(&mut self, request: &ProviderRuntimeLaunchRequest) -> ProviderLaunchOutcome;

    fn stop_and_join(
        &mut self,
        lease: &mut ProviderProcessLease,
    ) -> Result<ActiveProcessZeroSettlement, ProviderLaunchError>;

    /// Transfer a still-owned permit to the process registry/bridge recovery
    /// owner before a manager is dropped. The default is fail-closed: a
    /// production bridge must opt in with a real Job/PTY owner.
    fn retain_for_recovery(
        &mut self,
        _state: &ProviderSessionState,
        _lease: ProviderProcessLease,
    ) -> Result<(), ProviderLaunchError> {
        Err(ProviderLaunchError::BridgeUnavailable)
    }

    /// Reopen/reconcile an exact durable generation. A bridge must enumerate
    /// its registry; absence is not permission to synthesize a process.
    fn recover(
        &mut self,
        _state: &ProviderSessionState,
    ) -> Result<Option<ProviderProcessLease>, ProviderLaunchError> {
        Ok(None)
    }
}

pub(crate) mod sealed {
    pub trait ProviderProcessLauncher {}
    pub trait ProviderSessionStateStore {}
}

#[derive(Debug)]
pub enum ProviderLaunchOutcome {
    Started(ProviderProcessLease),
    Rejected(ProviderLaunchError),
    Failed {
        error: ProviderLaunchError,
        lease: Option<ProviderProcessLease>,
    },
}

/// Typed production fail-closed bridge. It never creates a synthetic process
/// identity or settlement while the Task 3 union is absent.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableProviderProcessLauncher;

impl sealed::ProviderProcessLauncher for UnavailableProviderProcessLauncher {}

impl ProviderProcessLauncher for UnavailableProviderProcessLauncher {
    fn launch(&mut self, _request: &ProviderRuntimeLaunchRequest) -> ProviderLaunchOutcome {
        ProviderLaunchOutcome::Rejected(ProviderLaunchError::BridgeUnavailable)
    }

    fn stop_and_join(
        &mut self,
        _lease: &mut ProviderProcessLease,
    ) -> Result<ActiveProcessZeroSettlement, ProviderLaunchError> {
        Err(ProviderLaunchError::BridgeUnavailable)
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct FixtureProviderLaunchSnapshot {
    launches: Vec<ProviderRuntimeLaunchRequest>,
    stopped: Vec<ProviderProcessId>,
    lease_drops: usize,
}

#[cfg(test)]
impl FixtureProviderLaunchSnapshot {
    pub fn launches(&self) -> &[ProviderRuntimeLaunchRequest] {
        &self.launches
    }

    pub fn stopped(&self) -> &[ProviderProcessId] {
        &self.stopped
    }

    pub const fn lease_drops(&self) -> usize {
        self.lease_drops
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
struct FixtureProviderLaunchState {
    snapshot: FixtureProviderLaunchSnapshot,
    next_process_id: u32,
    next_error: Option<ProviderLaunchError>,
    next_failure_after_start: Option<ProviderLaunchError>,
    stop_error: Option<ProviderLaunchError>,
    joined_active_process_zero: bool,
    next_fence_valid: bool,
    drop_observer: Arc<Mutex<usize>>,
    recovery: HashMap<RecoveryKey, ProviderProcessLease>,
    stopped_processes: HashSet<(u32, u64)>,
}

/// An injectable fake that can only mint the opaque host-issued capabilities
/// through this module. It is intentionally explicit in its name and does not
/// represent a production provider bridge.
#[cfg(test)]
#[derive(Debug)]
struct FixturePermitOwnership {
    drop_observer: Arc<Mutex<usize>>,
}

#[cfg(test)]
impl Drop for FixturePermitOwnership {
    fn drop(&mut self) {
        *self.drop_observer.lock().expect("fixture lease observer") += 1;
    }
}

#[cfg(test)]
impl ProviderPermitOwnership for FixturePermitOwnership {}

#[cfg(test)]
#[derive(Debug)]
struct FixtureZeroReceipt;

#[cfg(test)]
impl ProviderPermitOwnership for FixtureZeroReceipt {}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct FixtureProviderProcessLauncher {
    state: Arc<Mutex<FixtureProviderLaunchState>>,
}

#[cfg(test)]
impl Default for FixtureProviderProcessLauncher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl FixtureProviderProcessLauncher {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FixtureProviderLaunchState {
                joined_active_process_zero: true,
                next_fence_valid: true,
                ..FixtureProviderLaunchState::default()
            })),
        }
    }

    pub fn snapshot(&self) -> FixtureProviderLaunchSnapshot {
        let state = self.state.lock().expect("fixture provider state");
        let lease_drops = *state.drop_observer.lock().expect("fixture lease observer");
        FixtureProviderLaunchSnapshot {
            launches: state.snapshot.launches.clone(),
            stopped: state.snapshot.stopped.clone(),
            lease_drops,
        }
    }

    pub fn fail_next(&self, error: ProviderLaunchError) {
        self.state
            .lock()
            .expect("fixture provider state")
            .next_error = Some(error);
    }

    pub fn fail_next_after_start(&self, error: ProviderLaunchError) {
        self.state
            .lock()
            .expect("fixture provider state")
            .next_failure_after_start = Some(error);
    }

    pub fn set_stop_error(&self, error: Option<ProviderLaunchError>) {
        self.state
            .lock()
            .expect("fixture provider state")
            .stop_error = error;
    }

    pub fn set_joined_active_process_zero(&self, joined: bool) {
        self.state
            .lock()
            .expect("fixture provider state")
            .joined_active_process_zero = joined;
    }

    pub fn set_next_fence_valid(&self, valid: bool) {
        self.state
            .lock()
            .expect("fixture provider state")
            .next_fence_valid = valid;
    }

    fn fixture_fence(
        state: &mut FixtureProviderLaunchState,
        request: &ProviderRuntimeLaunchRequest,
    ) -> ManagedProcessFence {
        state.next_process_id = state.next_process_id.saturating_add(1).max(1);
        let process_id = ManagedProcessId::new(
            state.next_process_id,
            u64::from(state.next_process_id) + 100,
        )
        .expect("fixture process identity is non-zero");
        let root = crate::process::identity::ManagedProcessIdentity::new(
            process_id,
            request.launch_spec().executable().canonical_path(),
        )
        .expect("fixture executable identity is canonicalizable");
        let resource = if state.next_fence_valid {
            ResourceFence::new(
                request.launch_spec().resource_id(),
                request.launch_spec().generation(),
            )
        } else {
            ResourceFence::new(ResourceId::new(), request.launch_spec().generation())
        };
        ManagedProcessFence::new(
            resource,
            if state.next_fence_valid {
                ProcessOwner::Task(request.launch_spec().task_id())
            } else {
                ProcessOwner::Host
            },
            root,
        )
    }
}

#[cfg(test)]
impl sealed::ProviderProcessLauncher for FixtureProviderProcessLauncher {}

#[cfg(test)]
impl ProviderProcessLauncher for FixtureProviderProcessLauncher {
    fn launch(&mut self, request: &ProviderRuntimeLaunchRequest) -> ProviderLaunchOutcome {
        let mut state = self.state.lock().expect("fixture provider state");
        state.snapshot.launches.push(request.clone());
        if let Some(error) = state.next_error.take() {
            return ProviderLaunchOutcome::Rejected(error);
        }
        let fence = Self::fixture_fence(&mut state, request);
        let lease = ProviderManagedProcessPermit::from_registry(
            fence,
            FixturePermitOwnership {
                drop_observer: Arc::clone(&state.drop_observer),
            },
        );
        if let Some(error) = state.next_failure_after_start.take() {
            ProviderLaunchOutcome::Failed {
                error,
                lease: Some(lease),
            }
        } else {
            ProviderLaunchOutcome::Started(lease)
        }
    }

    fn stop_and_join(
        &mut self,
        lease: &mut ProviderProcessLease,
    ) -> Result<ActiveProcessZeroSettlement, ProviderLaunchError> {
        let mut state = self.state.lock().expect("fixture provider state");
        if let Some(error) = state.stop_error.take() {
            return Err(error);
        }
        if !state.joined_active_process_zero {
            return Err(ProviderLaunchError::ActiveProcessZeroRequired);
        }
        let process_id = lease.process_id();
        let process_key = (process_id.pid(), process_id.creation_time_100ns());
        if state.stopped_processes.insert(process_key) {
            state
                .snapshot
                .stopped
                .push(ProviderProcessId::from_managed(process_id));
        }
        Ok(JoinedActiveProcessZeroProof::from_registry(
            lease.fence().clone(),
            FixtureZeroReceipt,
        ))
    }

    fn retain_for_recovery(
        &mut self,
        state: &ProviderSessionState,
        lease: ProviderProcessLease,
    ) -> Result<(), ProviderLaunchError> {
        let key = RecoveryKey::from_state(state);
        self.state
            .lock()
            .expect("fixture provider state")
            .recovery
            .insert(key, lease);
        Ok(())
    }

    fn recover(
        &mut self,
        state: &ProviderSessionState,
    ) -> Result<Option<ProviderProcessLease>, ProviderLaunchError> {
        Ok(self
            .state
            .lock()
            .expect("fixture provider state")
            .recovery
            .remove(&RecoveryKey::from_state(state)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartProviderSessionRequest {
    agent: AgentSessionFacts,
    launch_spec: ProviderAdapterLaunchSpec,
    launch_proof: Option<ProviderAdapterLaunchProof>,
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
            launch_spec: ProviderAdapterLaunchSpec::unavailable_placeholder(
                executable,
                capabilities,
            ),
            launch_proof: None,
            mode,
        }
    }

    pub fn with_launch_proof(
        agent: AgentSessionFacts,
        launch_proof: ProviderAdapterLaunchProof,
        mode: ProviderSessionStartMode,
    ) -> Self {
        let launch_spec = launch_proof.spec.clone();
        Self {
            agent,
            launch_spec,
            launch_proof: Some(launch_proof),
            mode,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_launch_spec(
        agent: AgentSessionFacts,
        launch_spec: ProviderAdapterLaunchSpec,
        mode: ProviderSessionStartMode,
    ) -> Self {
        let provider_session_id = matches!(mode, ProviderSessionStartMode::ResumeExact)
            .then(|| agent.provider_session_id.clone())
            .flatten()
            .or_else(|| {
                matches!(mode, ProviderSessionStartMode::Open)
                    .then(|| agent.provider_session_id.clone())
                    .flatten()
            });
        let proof = ProviderAdapterLaunchProof::from_test(launch_spec, mode, provider_session_id)
            .expect("test launch proof");
        Self::with_launch_proof(agent, proof, mode)
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
        self.launch_spec.executable()
    }

    pub fn capabilities(&self) -> &ProviderCapabilities {
        self.launch_spec.capabilities()
    }

    pub fn launch_spec(&self) -> &ProviderAdapterLaunchSpec {
        &self.launch_spec
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
    AdapterLaunchSpecRequired,
    AdapterLaunchProofInvalid,
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
    ExactResumeUnavailable {
        provider: ProviderKind,
    },
    GenerationExhausted,
    ViewIdExhausted,
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
    SessionStartReplay,
    SessionStartProvenanceMismatch,
    ProviderSessionIdRebind {
        existing: ProviderSessionId,
        received: ProviderSessionId,
    },
    TerminalViewAlreadyAttached,
    SemanticViewLimitReached,
    ViewNotAttached,
    ViewFromDifferentRuntime,
    UnknownLeaked {
        agent_session_id: AgentSessionId,
        generation: u64,
    },
}

impl fmt::Display for ProviderSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AgentSessionNotFound(_) => formatter.write_str("agent session is not registered"),
            Self::AdapterLaunchSpecRequired => formatter.write_str(
                "provider launch requires an exact adapter launch specification",
            ),
            Self::AdapterLaunchProofInvalid => formatter.write_str(
                "provider adapter launch proof is invalid or bound to a different resume intent",
            ),
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
            Self::ExactResumeUnavailable { .. } => {
                formatter.write_str("exact provider resume is not supported by this capability snapshot")
            }
            Self::GenerationExhausted => {
                formatter.write_str("provider runtime generation exhausted")
            }
            Self::ViewIdExhausted => formatter.write_str("provider view ID space is exhausted"),
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
            Self::SessionStartReplay => {
                formatter.write_str("provider SessionStart token was already consumed")
            }
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
            Self::UnknownLeaked {
                agent_session_id,
                generation,
            } => write!(
                formatter,
                "provider generation {generation} for agent {agent_session_id:?} has unknown leaked ownership"
            ),
        }
    }
}

impl std::error::Error for ProviderSessionError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistedRuntimeLifecycle {
    Starting,
    Running,
    Stopping,
    LaunchFailed,
    UnknownLeaked,
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
    launch_spec: ProviderLaunchSpec,
    provider_session_id: Option<ProviderSessionId>,
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

    pub fn launch_spec(&self) -> &ProviderLaunchSpec {
        &self.launch_spec
    }

    pub fn provider_session_id(&self) -> Option<ProviderSessionId> {
        self.provider_session_id.clone()
    }
}

/// Durable state authority. Implementations must make a state write and its
/// recovery-journal receipt visible before the corresponding lifecycle
/// transition is exposed to callers. Session-start tokens are consumed by this
/// same authority, never by a manager-local set.
pub trait ProviderSessionStateStore: sealed::ProviderSessionStateStore {
    fn load(
        &self,
        agent_session_id: AgentSessionId,
    ) -> Result<Option<ProviderSessionState>, String>;
    fn persist(&mut self, state: ProviderSessionState) -> Result<(), String>;

    fn list_open_for_task(&self, _task_id: TaskId) -> Result<Vec<ProviderSessionState>, String> {
        Ok(Vec::new())
    }

    /// Enumerate durable generations whose recovery owner may still hold a
    /// root even after the latest state says Closed. This is separate from
    /// `list_open_for_task` so normal task enumeration keeps its lifecycle
    /// meaning.
    fn list_recovery_for_task(
        &self,
        _task_id: TaskId,
    ) -> Result<Vec<ProviderSessionState>, String> {
        Ok(Vec::new())
    }

    fn consume_session_start_token(
        &mut self,
        _provenance: &ProviderSessionStartProvenance,
    ) -> Result<bool, String> {
        Err("provider session store does not implement durable hook-token consumption".to_string())
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct InMemoryProviderSessionStateStore {
    states: HashMap<AgentSessionId, ProviderSessionState>,
    consumed_tokens: HashSet<Uuid>,
}

#[cfg(test)]
impl InMemoryProviderSessionStateStore {
    pub fn state(&self, agent_session_id: AgentSessionId) -> Option<&ProviderSessionState> {
        self.states.get(&agent_session_id)
    }
}

#[cfg(test)]
impl sealed::ProviderSessionStateStore for InMemoryProviderSessionStateStore {}

#[cfg(test)]
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

    fn list_open_for_task(&self, task_id: TaskId) -> Result<Vec<ProviderSessionState>, String> {
        Ok(self
            .states
            .values()
            .filter(|state| {
                state.task_id == task_id
                    && matches!(
                        state.lifecycle,
                        PersistedRuntimeLifecycle::Starting
                            | PersistedRuntimeLifecycle::Running
                            | PersistedRuntimeLifecycle::Stopping
                            | PersistedRuntimeLifecycle::UnknownLeaked
                    )
            })
            .cloned()
            .collect())
    }

    fn list_recovery_for_task(&self, task_id: TaskId) -> Result<Vec<ProviderSessionState>, String> {
        Ok(self
            .states
            .values()
            .filter(|state| {
                state.task_id == task_id && state.lifecycle == PersistedRuntimeLifecycle::Closed
            })
            .cloned()
            .collect())
    }

    fn consume_session_start_token(
        &mut self,
        provenance: &ProviderSessionStartProvenance,
    ) -> Result<bool, String> {
        Ok(self.consumed_tokens.insert(provenance.token_id()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedLaunchSpecWire {
    provider_kind: ProviderKind,
    executable_path: PathBuf,
    executable_sha256: [u8; 32],
    mode: PersistedLaunchModeWire,
    arguments: Vec<OsString>,
    cwd: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    capabilities: ProviderCapabilities,
    task_id: TaskId,
    resource_id: ResourceId,
    terminal_id: TerminalId,
    generation: u64,
    launch_nonce: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum PersistedLaunchModeWire {
    NewConversation,
    ResumeExact(ProviderSessionId),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProviderSessionStateWire {
    schema_version: u16,
    agent_session_id: AgentSessionId,
    task_id: TaskId,
    generation: u64,
    revision: u64,
    lifecycle: String,
    launch_nonce: Uuid,
    launch_spec: PersistedLaunchSpecWire,
    provider_session_id: Option<ProviderSessionId>,
}

const PROVIDER_SESSION_STATE_SCHEMA_VERSION: u16 = 1;

impl ProviderSessionState {
    fn encode(&self) -> Result<Vec<u8>, String> {
        let mode = match &self.launch_spec.mode {
            ProviderLaunchMode::NewConversation => PersistedLaunchModeWire::NewConversation,
            ProviderLaunchMode::ResumeExact(id) => PersistedLaunchModeWire::ResumeExact(id.clone()),
        };
        let wire = ProviderSessionStateWire {
            schema_version: PROVIDER_SESSION_STATE_SCHEMA_VERSION,
            agent_session_id: self.agent_session_id,
            task_id: self.task_id,
            generation: self.generation,
            revision: self.revision,
            lifecycle: persisted_lifecycle_name(self.lifecycle).to_string(),
            launch_nonce: self.launch_nonce.raw(),
            launch_spec: PersistedLaunchSpecWire {
                provider_kind: self.launch_spec.provider_kind,
                executable_path: self.launch_spec.executable.canonical_path().to_path_buf(),
                executable_sha256: *self.launch_spec.executable.sha256(),
                mode,
                arguments: self.launch_spec.arguments.clone(),
                cwd: self.launch_spec.cwd.clone(),
                environment: self.launch_spec.environment.clone(),
                capabilities: self.launch_spec.capabilities.clone(),
                task_id: self.launch_spec.task_id,
                resource_id: self.launch_spec.resource_id,
                terminal_id: self.launch_spec.terminal_id,
                generation: self.launch_spec.generation,
                launch_nonce: self.launch_spec.launch_nonce.raw(),
            },
            provider_session_id: self.provider_session_id.clone(),
        };
        serde_json::to_vec(&wire).map_err(|error| error.to_string())
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let wire: ProviderSessionStateWire =
            serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        if wire.schema_version != PROVIDER_SESSION_STATE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported provider session state schema {}",
                wire.schema_version
            ));
        }
        let persisted_launch = &wire.launch_spec;
        if wire.task_id != persisted_launch.task_id
            || wire.generation != persisted_launch.generation
            || wire.launch_nonce != persisted_launch.launch_nonce
            || persisted_launch.provider_kind != persisted_launch.capabilities.kind
        {
            return Err("persisted provider launch identity is inconsistent".to_string());
        }
        let executable = ProviderExecutable::new(
            persisted_launch.executable_path.clone(),
            persisted_launch.executable_sha256,
        )
        .map_err(|error| format!("persisted executable identity invalid: {error:?}"))?;
        let bounded_adapter_spec = ProviderAdapterLaunchSpec::bounded(
            executable.clone(),
            persisted_launch.arguments.clone(),
            persisted_launch.cwd.clone(),
            persisted_launch.environment.clone(),
            persisted_launch.capabilities.clone(),
        )
        .map_err(|error| format!("persisted launch spec is not bounded: {error:?}"))?;
        let mode = match persisted_launch.mode.clone() {
            PersistedLaunchModeWire::NewConversation => ProviderLaunchMode::NewConversation,
            PersistedLaunchModeWire::ResumeExact(id) => ProviderLaunchMode::ResumeExact(id),
        };
        if let ProviderLaunchMode::ResumeExact(expected) = &mode {
            if wire.provider_session_id.as_ref() != Some(expected) {
                return Err("persisted exact-resume identity is inconsistent".to_string());
            }
        }
        let lifecycle = persisted_lifecycle_from_name(&wire.lifecycle)?;
        let launch_spec = ProviderLaunchSpec {
            provider_kind: persisted_launch.provider_kind,
            executable: bounded_adapter_spec.executable,
            mode,
            arguments: bounded_adapter_spec.arguments,
            cwd: bounded_adapter_spec.cwd,
            environment: bounded_adapter_spec.environment,
            capabilities: bounded_adapter_spec.capabilities,
            task_id: persisted_launch.task_id,
            resource_id: persisted_launch.resource_id,
            terminal_id: persisted_launch.terminal_id,
            generation: persisted_launch.generation,
            launch_nonce: LaunchNonce::from_raw(persisted_launch.launch_nonce),
        };
        Ok(Self {
            agent_session_id: wire.agent_session_id,
            task_id: wire.task_id,
            generation: wire.generation,
            revision: wire.revision,
            lifecycle,
            launch_nonce: LaunchNonce::from_raw(wire.launch_nonce),
            launch_spec,
            provider_session_id: wire.provider_session_id,
        })
    }
}

fn persisted_lifecycle_name(lifecycle: PersistedRuntimeLifecycle) -> &'static str {
    match lifecycle {
        PersistedRuntimeLifecycle::Starting => "starting",
        PersistedRuntimeLifecycle::Running => "running",
        PersistedRuntimeLifecycle::Stopping => "stopping",
        PersistedRuntimeLifecycle::LaunchFailed => "launch_failed",
        PersistedRuntimeLifecycle::UnknownLeaked => "unknown_leaked",
        PersistedRuntimeLifecycle::Replaced => "replaced",
        PersistedRuntimeLifecycle::Closed => "closed",
    }
}

fn persisted_lifecycle_from_name(value: &str) -> Result<PersistedRuntimeLifecycle, String> {
    match value {
        "starting" => Ok(PersistedRuntimeLifecycle::Starting),
        "running" => Ok(PersistedRuntimeLifecycle::Running),
        "stopping" => Ok(PersistedRuntimeLifecycle::Stopping),
        "launch_failed" => Ok(PersistedRuntimeLifecycle::LaunchFailed),
        "unknown_leaked" => Ok(PersistedRuntimeLifecycle::UnknownLeaked),
        "replaced" => Ok(PersistedRuntimeLifecycle::Replaced),
        "closed" => Ok(PersistedRuntimeLifecycle::Closed),
        other => Err(format!("unknown provider session lifecycle {other}")),
    }
}

/// File-backed provider-session state and recovery journal. The latest state
/// and every transition receipt are committed in one SQLite transaction, so a
/// crash can expose either the previous authoritative state or the complete
/// next state, never an unjournaled effect.
pub struct SqliteProviderSessionStateStore {
    connection: Connection,
}

impl fmt::Debug for SqliteProviderSessionStateStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SqliteProviderSessionStateStore(<durable>)")
    }
}

impl SqliteProviderSessionStateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let connection = Connection::open(path).map_err(|error| error.to_string())?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| error.to_string())?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 CREATE TABLE IF NOT EXISTS provider_session_states (
                   agent_session_id TEXT PRIMARY KEY,
                   revision INTEGER NOT NULL,
                   lifecycle TEXT NOT NULL,
                   state_json BLOB NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS provider_session_journal (
                   sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                   agent_session_id TEXT NOT NULL,
                   revision INTEGER NOT NULL,
                   lifecycle TEXT NOT NULL,
                   state_json BLOB NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS provider_session_recovery_ownership (
                   agent_session_id TEXT NOT NULL,
                   generation INTEGER NOT NULL,
                   launch_nonce TEXT NOT NULL,
                   ownership_state TEXT NOT NULL,
                   revision INTEGER NOT NULL,
                   PRIMARY KEY (agent_session_id, generation, launch_nonce)
                 );
                 CREATE TABLE IF NOT EXISTS provider_session_start_tokens (
                   token_id TEXT PRIMARY KEY,
                   task_id TEXT NOT NULL,
                   agent_session_id TEXT NOT NULL,
                   generation INTEGER NOT NULL,
                   launch_nonce TEXT NOT NULL,
                   provider_kind TEXT NOT NULL,
                   provider_session_id TEXT NOT NULL
                 );",
            )
            .map_err(|error| error.to_string())?;
        Ok(Self { connection })
    }

    fn persist_transaction(
        transaction: &Transaction<'_>,
        state: &ProviderSessionState,
        bytes: &[u8],
    ) -> Result<(), String> {
        let agent_id = state.agent_session_id.to_string();
        let current: Option<i64> = transaction
            .query_row(
                "SELECT revision FROM provider_session_states WHERE agent_session_id = ?1",
                params![agent_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if current.is_some_and(|revision| state.revision as i64 <= revision) {
            return Err("provider session state revision is not monotonic".to_string());
        }
        transaction
            .execute(
                "INSERT INTO provider_session_states
                   (agent_session_id, revision, lifecycle, state_json)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(agent_session_id) DO UPDATE SET
                   revision = excluded.revision,
                   lifecycle = excluded.lifecycle,
                   state_json = excluded.state_json",
                params![
                    state.agent_session_id.to_string(),
                    state.revision as i64,
                    persisted_lifecycle_name(state.lifecycle),
                    bytes,
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT INTO provider_session_journal
                   (agent_session_id, revision, lifecycle, state_json)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    state.agent_session_id.to_string(),
                    state.revision as i64,
                    persisted_lifecycle_name(state.lifecycle),
                    bytes,
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT INTO provider_session_recovery_ownership
                   (agent_session_id, generation, launch_nonce, ownership_state, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(agent_session_id, generation, launch_nonce) DO UPDATE SET
                   ownership_state = excluded.ownership_state,
                   revision = excluded.revision",
                params![
                    state.agent_session_id.to_string(),
                    state.generation as i64,
                    state.launch_nonce.raw().to_string(),
                    persisted_lifecycle_name(state.lifecycle),
                    state.revision as i64,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

impl sealed::ProviderSessionStateStore for SqliteProviderSessionStateStore {}

impl ProviderSessionStateStore for SqliteProviderSessionStateStore {
    fn load(
        &self,
        agent_session_id: AgentSessionId,
    ) -> Result<Option<ProviderSessionState>, String> {
        let bytes: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT state_json FROM provider_session_states WHERE agent_session_id = ?1",
                params![agent_session_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        bytes
            .map(|bytes| ProviderSessionState::decode(&bytes))
            .transpose()
    }

    fn persist(&mut self, state: ProviderSessionState) -> Result<(), String> {
        let bytes = state.encode()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        Self::persist_transaction(&transaction, &state, &bytes)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    fn list_open_for_task(&self, task_id: TaskId) -> Result<Vec<ProviderSessionState>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT state_json FROM provider_session_states
                 WHERE lifecycle IN ('starting', 'running', 'stopping', 'unknown_leaked')",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|error| error.to_string())?;
        let mut states = Vec::new();
        for row in rows {
            let state = ProviderSessionState::decode(&row.map_err(|error| error.to_string())?)?;
            if state.task_id == task_id {
                states.push(state);
            }
        }
        Ok(states)
    }

    fn list_recovery_for_task(&self, task_id: TaskId) -> Result<Vec<ProviderSessionState>, String> {
        let mut statement = self
            .connection
            .prepare("SELECT state_json FROM provider_session_states")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|error| error.to_string())?;
        let mut states = Vec::new();
        for row in rows {
            let state = ProviderSessionState::decode(&row.map_err(|error| error.to_string())?)?;
            if state.task_id == task_id && state.lifecycle == PersistedRuntimeLifecycle::Closed {
                states.push(state);
            }
        }
        Ok(states)
    }

    fn consume_session_start_token(
        &mut self,
        provenance: &ProviderSessionStartProvenance,
    ) -> Result<bool, String> {
        let correlation = provenance.correlation();
        let token_id = provenance.token_id().to_string();
        // Serialize the read/validate/insert sequence so two independent
        // managers cannot both accept the same opaque hook token during a
        // restart race.
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let existing: Option<(String, String, i64, String, String, String)> = transaction
            .query_row(
                "SELECT task_id, agent_session_id, generation, launch_nonce,
                        provider_kind, provider_session_id
                 FROM provider_session_start_tokens WHERE token_id = ?1",
                params![token_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some(existing) = existing {
            let matches = existing.0 == correlation.task_id.to_string()
                && existing.1 == correlation.agent_session_id.to_string()
                && existing.2 == correlation.generation as i64
                && existing.3 == correlation.launch_nonce.raw().to_string()
                && existing.4 == correlation.provider_kind.wire_name()
                && existing.5 == provenance.provider_session_id().as_str();
            if !matches {
                return Err("provider session start token provenance mismatch".to_string());
            }
            transaction.commit().map_err(|error| error.to_string())?;
            return Ok(false);
        }
        transaction
            .execute(
                "INSERT INTO provider_session_start_tokens
                   (token_id, task_id, agent_session_id, generation, launch_nonce,
                    provider_kind, provider_session_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    provenance.token_id().to_string(),
                    correlation.task_id.to_string(),
                    correlation.agent_session_id.to_string(),
                    correlation.generation as i64,
                    correlation.launch_nonce.raw().to_string(),
                    correlation.provider_kind.wire_name(),
                    provenance.provider_session_id().as_str(),
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(true)
    }
}

/// One-shot authenticated current-generation SessionStart token. The token is
/// deliberately non-Clone and has no public constructor. Task 4.1b's hook
/// registry (or the explicit fixture issuer) must mint it after authenticating
/// the current launch nonce and provider event; the manager consumes it once.
#[derive(Debug, PartialEq, Eq)]
pub struct ProviderSessionStartProvenance {
    correlation: RuntimeCorrelation,
    provider_session_id: ProviderSessionId,
    token_id: Uuid,
}

impl ProviderSessionStartProvenance {
    pub fn correlation(&self) -> RuntimeCorrelation {
        self.correlation
    }
    pub fn provider_session_id(&self) -> &ProviderSessionId {
        &self.provider_session_id
    }

    fn token_id(&self) -> Uuid {
        self.token_id
    }

    /// Test-only handoff for the fixture issuer. The production Task 4.1b
    /// hook issuer remains unavailable until its authenticated identity union
    /// is joined; no caller-facing factory exists.
    #[cfg(test)]
    fn mint(correlation: RuntimeCorrelation, provider_session_id: ProviderSessionId) -> Self {
        Self {
            correlation,
            provider_session_id,
            token_id: Uuid::now_v7(),
        }
    }
}

/// A deterministic, injectable identity issuer for fixture tests. It is not a
/// provider hook relay and is never used by the production unavailable bridge.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct FixtureProviderSessionStartIssuer {
    issued: Arc<Mutex<HashSet<Uuid>>>,
    last_by_correlation: Arc<Mutex<HashMap<RuntimeCorrelation, Uuid>>>,
}

#[cfg(test)]
impl FixtureProviderSessionStartIssuer {
    pub fn issue(
        &self,
        correlation: RuntimeCorrelation,
        provider_session_id: ProviderSessionId,
    ) -> ProviderSessionStartProvenance {
        let token = ProviderSessionStartProvenance::mint(correlation, provider_session_id);
        self.issued
            .lock()
            .expect("fixture identity issuer")
            .insert(token.token_id());
        self.last_by_correlation
            .lock()
            .expect("fixture identity issuer")
            .insert(correlation, token.token_id());
        token
    }

    /// Test-only replay seam. A real hook relay never exposes token bytes back
    /// to callers; this exists solely to prove the manager's one-shot fence.
    pub fn replay(
        &self,
        correlation: RuntimeCorrelation,
        provider_session_id: ProviderSessionId,
    ) -> Option<ProviderSessionStartProvenance> {
        let token_id = self
            .last_by_correlation
            .lock()
            .expect("fixture identity issuer")
            .get(&correlation)
            .copied()?;
        Some(ProviderSessionStartProvenance {
            correlation,
            provider_session_id,
            token_id,
        })
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
    launch_spec: ProviderLaunchSpec,
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
                launch_spec: request.launch_spec.clone(),
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
        self.state.lock().unwrap().launch_spec.executable.clone()
    }
    pub fn capabilities(&self) -> ProviderCapabilities {
        self.state.lock().unwrap().launch_spec.capabilities.clone()
    }
    pub fn launch_spec(&self) -> ProviderLaunchSpec {
        self.state.lock().unwrap().launch_spec.clone()
    }
    pub(crate) fn fence(&self) -> ManagedProcessFence {
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
            RuntimeLifecycle::Running | RuntimeLifecycle::Exited => {
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
#[derive(Debug)]
struct LeaseSlot {
    lease: ProviderProcessLease,
    state: ProviderSessionState,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct RecoveryKey {
    agent_session_id: AgentSessionId,
    generation: u64,
    launch_nonce: LaunchNonce,
}

#[cfg(test)]
impl RecoveryKey {
    fn from_state(state: &ProviderSessionState) -> Self {
        Self {
            agent_session_id: state.agent_session_id,
            generation: state.generation,
            launch_nonce: state.launch_nonce,
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSessionCrashPoint {
    AfterStartingJournal,
    AfterPermitAcquired,
    BeforeStoppingJournal,
    AfterJoinedZero,
    AfterClosedJournal,
}

pub struct ProviderSessionManager<L, S>
where
    L: ProviderProcessLauncher,
    S: ProviderSessionStateStore,
{
    launcher: L,
    state_store: S,
    current: HashMap<AgentSessionId, ProviderRuntime>,
    leases: HashMap<AgentSessionId, LeaseSlot>,
    pending_launches: HashMap<AgentSessionId, ProviderSessionState>,
    next_generation: HashMap<AgentSessionId, u64>,
    next_state_revision: HashMap<AgentSessionId, u64>,
    next_view_id: u64,
    _store_contract_marker: std::marker::PhantomData<fn() -> S>,
    #[cfg(test)]
    crash_point: Option<ProviderSessionCrashPoint>,
}

#[cfg(test)]
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
            pending_launches: HashMap::new(),
            next_generation: HashMap::new(),
            next_state_revision: HashMap::new(),
            next_view_id: 0,
            _store_contract_marker: std::marker::PhantomData,
            #[cfg(test)]
            crash_point: None,
        }
    }

    pub fn current(&self, agent_session_id: AgentSessionId) -> Option<ProviderRuntime> {
        self.current.get(&agent_session_id).cloned()
    }

    pub fn start(
        &mut self,
        request: StartProviderSessionRequest,
    ) -> Result<ProviderRuntime, ProviderSessionError> {
        self.ensure_request_admissible(&request)?;
        let agent_id = request.agent.id;
        let persisted = self.load_state(agent_id)?;
        let provider_session_id = provider_session_id_for_request(&request, persisted.as_ref())?;
        // Resolve exact-resume capability before any recovery/teardown effect
        // can occur for a persisted generation.
        let mode = resolve_launch_mode(&request, provider_session_id.clone())?;
        if !request
            .launch_proof
            .as_ref()
            .expect("admissible request has a launch proof")
            .matches_resolved_mode(&mode)
        {
            return Err(ProviderSessionError::AdapterLaunchProofInvalid);
        }
        let launch_provider_session_id = match &mode {
            ProviderLaunchMode::NewConversation => None,
            ProviderLaunchMode::ResumeExact(provider_session_id) => {
                Some(provider_session_id.clone())
            }
        };
        if let Some(existing) = self.current.get(&agent_id) {
            match existing.lifecycle() {
                RuntimeLifecycle::Running | RuntimeLifecycle::Stopping => {
                    return Err(ProviderSessionError::SessionAlreadyRunning(agent_id))
                }
                RuntimeLifecycle::Closed => {
                    return Err(ProviderSessionError::SessionClosed(agent_id))
                }
                RuntimeLifecycle::Exited | RuntimeLifecycle::Replaced => {
                    let existing = existing.clone();
                    if self.leases.contains_key(&agent_id) {
                        self.settle_runtime(
                            agent_id,
                            &existing,
                            PersistedRuntimeLifecycle::Replaced,
                        )?;
                    }
                }
            }
        }

        if let Some(state) = &persisted {
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
                    | PersistedRuntimeLifecycle::UnknownLeaked
            ) && self.current.get(&agent_id).is_none()
            {
                self.recover_persisted_state(state)?;
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
            executable: request.launch_spec.executable.clone(),
            mode: mode.clone(),
            arguments: request.launch_spec.arguments.clone(),
            cwd: request.launch_spec.cwd.clone(),
            environment: request.launch_spec.environment.clone(),
            capabilities: request.launch_spec.capabilities.clone(),
            task_id: request.agent.task_id,
            resource_id,
            terminal_id,
            generation,
            launch_nonce: correlation.launch_nonce,
        };
        let launch_request = ProviderRuntimeLaunchRequest {
            correlation,
            launch_spec,
        };
        let starting_state = self.state_for_launch(
            &launch_request,
            PersistedRuntimeLifecycle::Starting,
            launch_provider_session_id.clone(),
        );
        self.persist_state(starting_state.clone())?;
        self.pending_launches
            .insert(agent_id, starting_state.clone());
        #[cfg(test)]
        self.crash_if(ProviderSessionCrashPoint::AfterStartingJournal);
        let outcome = self.launcher.launch(&launch_request);
        let lease = match outcome {
            ProviderLaunchOutcome::Started(lease) => lease,
            ProviderLaunchOutcome::Rejected(error) => {
                self.pending_launches.remove(&agent_id);
                self.persist_state_with_lifecycle(
                    &starting_state,
                    PersistedRuntimeLifecycle::LaunchFailed,
                )?;
                return Err(self.map_launch_error(&mode, error));
            }
            ProviderLaunchOutcome::Failed { error, lease } => {
                self.pending_launches.remove(&agent_id);
                return self.handle_failed_launch(
                    &launch_request,
                    starting_state,
                    mode,
                    error,
                    lease,
                );
            }
        };
        if let Err(error) = validate_lease(&launch_request, &lease) {
            self.pending_launches.remove(&agent_id);
            return self.handle_failed_launch(
                &launch_request,
                starting_state,
                mode,
                match error {
                    ProviderSessionError::LaunchFailed(error) => error,
                    _ => ProviderLaunchError::ProcessFenceMismatch,
                },
                Some(lease),
            );
        }
        self.leases.insert(
            agent_id,
            LeaseSlot {
                lease,
                state: starting_state.clone(),
            },
        );
        #[cfg(test)]
        self.crash_if(ProviderSessionCrashPoint::AfterPermitAcquired);
        let running_state = self.state_for_launch(
            &launch_request,
            PersistedRuntimeLifecycle::Running,
            launch_provider_session_id,
        );
        if let Err(error) = self.persist_state(running_state.clone()) {
            self.pending_launches.remove(&agent_id);
            let slot = self
                .leases
                .remove(&agent_id)
                .expect("launch lease inserted before running journal");
            let _ = self.handle_failed_launch(
                &launch_request,
                starting_state,
                mode,
                ProviderLaunchError::StopFailed,
                Some(slot.lease),
            );
            return Err(error);
        }
        self.leases
            .get_mut(&agent_id)
            .expect("launch lease inserted")
            .state = running_state;
        self.pending_launches.remove(&agent_id);
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
                .lease
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
        self.ensure_request_admissible(&request)?;
        let agent_id = request.agent.id;
        let persisted = self.load_state(agent_id)?;
        let provider_session_id = provider_session_id_for_request(&request, persisted.as_ref())?;
        let mode = resolve_launch_mode(&request, provider_session_id)?;
        if !request
            .launch_proof
            .as_ref()
            .expect("admissible request has a launch proof")
            .matches_resolved_mode(&mode)
        {
            return Err(ProviderSessionError::AdapterLaunchProofInvalid);
        }
        if let Some(existing) = self.current.get(&agent_id).cloned() {
            if existing.task_id() != request.agent.task_id {
                return Err(ProviderSessionError::WrongTask {
                    expected: existing.task_id(),
                    actual: request.agent.task_id,
                });
            }
            self.settle_runtime(agent_id, &existing, PersistedRuntimeLifecycle::Replaced)?;
        } else if let Some(state) = persisted {
            if matches!(
                state.lifecycle,
                PersistedRuntimeLifecycle::Starting
                    | PersistedRuntimeLifecycle::Running
                    | PersistedRuntimeLifecycle::Stopping
                    | PersistedRuntimeLifecycle::UnknownLeaked
            ) {
                self.recover_persisted_state(&state)?;
            }
        }
        self.start(request)
    }

    pub fn attach_terminal_view(
        &mut self,
        correlation: RuntimeCorrelation,
    ) -> Result<ProviderView, ProviderSessionError> {
        let runtime = self.live_runtime(correlation)?;
        let view_id = self.allocate_view_id()?;
        runtime.attach_terminal_view(view_id)
    }

    pub fn subscribe_semantic(
        &mut self,
        correlation: RuntimeCorrelation,
    ) -> Result<ProviderView, ProviderSessionError> {
        let runtime = self.live_runtime(correlation)?;
        let view_id = self.allocate_view_id()?;
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
        if !self
            .state_store
            .consume_session_start_token(&provenance)
            .map_err(ProviderSessionError::StateStore)?
        {
            return Err(ProviderSessionError::SessionStartReplay);
        }
        let accepted = runtime.accept_provider_session_start(&provenance)?;
        let state = self.state_for_runtime(&runtime, PersistedRuntimeLifecycle::Running);
        self.persist_state(state)?;
        let persisted = self
            .state_store
            .load(runtime.agent_session_id())
            .map_err(ProviderSessionError::StateStore)?
            .unwrap_or_else(|| {
                self.state_for_runtime(&runtime, PersistedRuntimeLifecycle::Running)
            });
        if let Some(slot) = self.leases.get_mut(&runtime.agent_session_id()) {
            slot.state = persisted;
        }
        Ok(accepted)
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
                Some(state) if state.lifecycle == PersistedRuntimeLifecycle::Closed => {
                    self.reconcile_closed_state(&state)
                }
                Some(state)
                    if matches!(
                        state.lifecycle,
                        PersistedRuntimeLifecycle::Starting
                            | PersistedRuntimeLifecycle::Running
                            | PersistedRuntimeLifecycle::Stopping
                    ) =>
                {
                    self.recover_persisted_state(&state)?;
                    self.persist_state_with_lifecycle(&state, PersistedRuntimeLifecycle::Closed)
                }
                Some(state) if state.lifecycle == PersistedRuntimeLifecycle::UnknownLeaked => {
                    self.recover_persisted_state(&state)?;
                    self.persist_state_with_lifecycle(&state, PersistedRuntimeLifecycle::Closed)
                }
                _ => Err(ProviderSessionError::AgentSessionNotFound(agent_session_id)),
            };
        };
        if runtime.lifecycle() == RuntimeLifecycle::Closed {
            if self.leases.contains_key(&agent_session_id) {
                return self.reconcile_closed_runtime(agent_session_id, &runtime);
            }
            return Ok(());
        }
        self.settle_runtime(
            agent_session_id,
            &runtime,
            PersistedRuntimeLifecycle::Closed,
        )
    }

    fn reconcile_closed_runtime(
        &mut self,
        agent_session_id: AgentSessionId,
        runtime: &ProviderRuntime,
    ) -> Result<(), ProviderSessionError> {
        let slot = self.leases.get_mut(&agent_session_id).ok_or(
            ProviderSessionError::SettlementRequired {
                agent_session_id,
                generation: runtime.generation(),
            },
        )?;
        let settlement = self
            .launcher
            .stop_and_join(&mut slot.lease)
            .map_err(ProviderSessionError::StopFailed)?;
        if !settlement.matches_fence(&runtime.fence()) {
            return Err(ProviderSessionError::SettlementFenceMismatch);
        }
        self.persist_state_for_runtime(runtime, PersistedRuntimeLifecycle::Closed)?;
        self.leases.remove(&agent_session_id);
        Ok(())
    }

    /// A crash can occur after the authoritative Closed journal commit but
    /// before the recovery owner removes its permit. Reconcile that leftover
    /// idempotently; a second close sees no permit and remains successful.
    fn reconcile_closed_state(
        &mut self,
        state: &ProviderSessionState,
    ) -> Result<(), ProviderSessionError> {
        let Some(mut lease) = self
            .launcher
            .recover(state)
            .map_err(ProviderSessionError::StopFailed)?
        else {
            return Ok(());
        };
        let request = ProviderRuntimeLaunchRequest {
            correlation: RuntimeCorrelation {
                task_id: state.task_id,
                agent_session_id: state.agent_session_id,
                provider_kind: state.launch_spec.provider_kind,
                generation: state.generation,
                launch_nonce: state.launch_nonce,
            },
            launch_spec: state.launch_spec.clone(),
        };
        if validate_lease(&request, &lease).is_err() {
            self.retain_unknown_lease(state, lease)?;
            return Err(ProviderSessionError::SettlementFenceMismatch);
        }
        let settlement = match self.launcher.stop_and_join(&mut lease) {
            Ok(settlement) => settlement,
            Err(error) => {
                self.retain_unknown_lease(state, lease)?;
                return Err(ProviderSessionError::StopFailed(error));
            }
        };
        if !settlement.matches_permit(&lease) {
            self.retain_unknown_lease(state, lease)?;
            return Err(ProviderSessionError::SettlementFenceMismatch);
        }
        self.persist_state_with_lifecycle(state, PersistedRuntimeLifecycle::Closed)
    }

    pub fn close_task(&mut self, task_id: TaskId) -> Result<(), ProviderSessionError> {
        let mut agent_ids: Vec<_> = self
            .current
            .iter()
            .filter_map(|(agent_id, runtime)| (runtime.task_id() == task_id).then_some(*agent_id))
            .collect();
        for state in self
            .state_store
            .list_open_for_task(task_id)
            .map_err(ProviderSessionError::StateStore)?
        {
            if !agent_ids.contains(&state.agent_session_id()) {
                agent_ids.push(state.agent_session_id());
            }
        }
        for state in self
            .state_store
            .list_recovery_for_task(task_id)
            .map_err(ProviderSessionError::StateStore)?
        {
            if !agent_ids.contains(&state.agent_session_id()) {
                agent_ids.push(state.agent_session_id());
            }
        }
        for agent_id in agent_ids {
            self.close_agent_session(agent_id)?;
        }
        Ok(())
    }

    fn ensure_request_admissible(
        &self,
        request: &StartProviderSessionRequest,
    ) -> Result<(), ProviderSessionError> {
        let Some(proof) = request.launch_proof.as_ref() else {
            return Err(ProviderSessionError::AdapterLaunchSpecRequired);
        };
        if !proof.matches_request(request.mode, request.agent.provider_session_id.as_ref()) {
            return Err(ProviderSessionError::AdapterLaunchProofInvalid);
        }
        if request.agent.lifecycle != crate::domain::AgentSessionLifecycle::Open {
            return Err(ProviderSessionError::SessionClosed(request.agent.id));
        }
        if request.agent.provider_kind != request.launch_spec.capabilities.kind {
            return Err(ProviderSessionError::ProviderKindMismatch {
                agent: request.agent.provider_kind,
                capabilities: request.launch_spec.capabilities.kind,
            });
        }
        request
            .launch_spec
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
            let revision = self
                .next_state_revision
                .entry(agent_session_id)
                .or_insert(0);
            *revision = (*revision).max(state.revision);
        }
        Ok(state)
    }

    fn persist_state(
        &mut self,
        mut state: ProviderSessionState,
    ) -> Result<(), ProviderSessionError> {
        let revision = self
            .next_state_revision
            .entry(state.agent_session_id)
            .or_insert(0);
        *revision = revision
            .checked_add(1)
            .ok_or(ProviderSessionError::GenerationExhausted)?;
        state.revision = *revision;
        self.state_store
            .persist(state)
            .map_err(ProviderSessionError::StateStore)
    }

    fn settle_runtime(
        &mut self,
        agent_id: AgentSessionId,
        runtime: &ProviderRuntime,
        final_state: PersistedRuntimeLifecycle,
    ) -> Result<(), ProviderSessionError> {
        match runtime.lifecycle() {
            RuntimeLifecycle::Running | RuntimeLifecycle::Stopping | RuntimeLifecycle::Exited => {}
            lifecycle => return Err(ProviderSessionError::RuntimeNotLive { lifecycle }),
        }
        #[cfg(test)]
        self.crash_if(ProviderSessionCrashPoint::BeforeStoppingJournal);
        self.persist_state_for_runtime(runtime, PersistedRuntimeLifecycle::Stopping)?;
        runtime.mark_stopping()?;
        let slot =
            self.leases
                .get_mut(&agent_id)
                .ok_or(ProviderSessionError::SettlementRequired {
                    agent_session_id: agent_id,
                    generation: runtime.generation(),
                })?;
        let settlement = self
            .launcher
            .stop_and_join(&mut slot.lease)
            .map_err(ProviderSessionError::StopFailed)?;
        #[cfg(test)]
        self.crash_if(ProviderSessionCrashPoint::AfterJoinedZero);
        if !settlement.matches_fence(&runtime.fence()) {
            return Err(ProviderSessionError::SettlementFenceMismatch);
        }
        self.persist_state_for_runtime(runtime, final_state)?;
        match final_state {
            PersistedRuntimeLifecycle::Replaced => runtime.mark_replaced(),
            PersistedRuntimeLifecycle::Closed => runtime.mark_closed(),
            _ => unreachable!("settlement final state must be Replaced or Closed"),
        }
        #[cfg(test)]
        if final_state == PersistedRuntimeLifecycle::Closed {
            self.crash_if(ProviderSessionCrashPoint::AfterClosedJournal);
        }
        self.leases.remove(&agent_id);
        Ok(())
    }

    fn state_for_launch(
        &self,
        request: &ProviderRuntimeLaunchRequest,
        lifecycle: PersistedRuntimeLifecycle,
        provider_session_id: Option<ProviderSessionId>,
    ) -> ProviderSessionState {
        ProviderSessionState {
            agent_session_id: request.correlation.agent_session_id,
            task_id: request.correlation.task_id,
            generation: request.correlation.generation,
            revision: 0,
            lifecycle,
            launch_nonce: request.correlation.launch_nonce,
            launch_spec: request.launch_spec.clone(),
            provider_session_id,
        }
    }

    fn state_for_runtime(
        &self,
        runtime: &ProviderRuntime,
        lifecycle: PersistedRuntimeLifecycle,
    ) -> ProviderSessionState {
        ProviderSessionState {
            agent_session_id: runtime.agent_session_id(),
            task_id: runtime.task_id(),
            generation: runtime.generation(),
            revision: 0,
            lifecycle,
            launch_nonce: runtime.launch_nonce(),
            launch_spec: runtime.launch_spec(),
            provider_session_id: runtime.provider_session_id(),
        }
    }

    fn persist_state_with_lifecycle(
        &mut self,
        state: &ProviderSessionState,
        lifecycle: PersistedRuntimeLifecycle,
    ) -> Result<(), ProviderSessionError> {
        let mut next = state.clone();
        next.lifecycle = lifecycle;
        next.revision = 0;
        self.persist_state(next)
    }

    fn persist_state_for_runtime(
        &mut self,
        runtime: &ProviderRuntime,
        lifecycle: PersistedRuntimeLifecycle,
    ) -> Result<(), ProviderSessionError> {
        self.persist_state(self.state_for_runtime(runtime, lifecycle))
    }

    fn recover_persisted_state(
        &mut self,
        state: &ProviderSessionState,
    ) -> Result<(), ProviderSessionError> {
        let Some(mut lease) = self
            .launcher
            .recover(state)
            .map_err(ProviderSessionError::StopFailed)?
        else {
            if state.lifecycle != PersistedRuntimeLifecycle::UnknownLeaked {
                self.persist_state_with_lifecycle(state, PersistedRuntimeLifecycle::UnknownLeaked)?;
            }
            return Err(ProviderSessionError::UnknownLeaked {
                agent_session_id: state.agent_session_id,
                generation: state.generation,
            });
        };
        let request = ProviderRuntimeLaunchRequest {
            correlation: RuntimeCorrelation {
                task_id: state.task_id,
                agent_session_id: state.agent_session_id,
                provider_kind: state.launch_spec.provider_kind,
                generation: state.generation,
                launch_nonce: state.launch_nonce,
            },
            launch_spec: state.launch_spec.clone(),
        };
        if validate_lease(&request, &lease).is_err() {
            let _ = self.launcher.retain_for_recovery(state, lease);
            return Err(ProviderSessionError::SettlementFenceMismatch);
        }
        let settlement = match self.launcher.stop_and_join(&mut lease) {
            Ok(settlement) => settlement,
            Err(error) => {
                let _ = self.launcher.retain_for_recovery(state, lease);
                return Err(ProviderSessionError::StopFailed(error));
            }
        };
        if !settlement.matches_permit(&lease) {
            let _ = self.launcher.retain_for_recovery(state, lease);
            return Err(ProviderSessionError::SettlementFenceMismatch);
        }
        self.persist_state_with_lifecycle(state, PersistedRuntimeLifecycle::Replaced)
    }

    fn handle_failed_launch(
        &mut self,
        request: &ProviderRuntimeLaunchRequest,
        state: ProviderSessionState,
        mode: ProviderLaunchMode,
        error: ProviderLaunchError,
        lease: Option<ProviderProcessLease>,
    ) -> Result<ProviderRuntime, ProviderSessionError> {
        let Some(mut lease) = lease else {
            self.persist_state_with_lifecycle(&state, PersistedRuntimeLifecycle::UnknownLeaked)?;
            return Err(self.map_launch_error(&mode, error));
        };
        if validate_lease(request, &lease).is_err() {
            self.retain_unknown_lease(&state, lease)?;
            return Err(ProviderSessionError::LaunchFailed(
                ProviderLaunchError::ProcessFenceMismatch,
            ));
        }
        let settlement = match self.launcher.stop_and_join(&mut lease) {
            Ok(settlement) => settlement,
            Err(stop_error) => {
                self.retain_unknown_lease(&state, lease)?;
                return Err(ProviderSessionError::StopFailed(stop_error));
            }
        };
        if !settlement.matches_permit(&lease) {
            self.retain_unknown_lease(&state, lease)?;
            return Err(ProviderSessionError::SettlementFenceMismatch);
        }
        self.persist_state_with_lifecycle(&state, PersistedRuntimeLifecycle::LaunchFailed)?;
        Err(self.map_launch_error(&mode, error))
    }

    fn retain_unknown_lease(
        &mut self,
        state: &ProviderSessionState,
        lease: ProviderProcessLease,
    ) -> Result<(), ProviderSessionError> {
        let _ = self.launcher.retain_for_recovery(state, lease);
        self.persist_state_with_lifecycle(state, PersistedRuntimeLifecycle::UnknownLeaked)
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

    fn allocate_view_id(&mut self) -> Result<u64, ProviderSessionError> {
        self.next_view_id = self
            .next_view_id
            .checked_add(1)
            .ok_or(ProviderSessionError::ViewIdExhausted)?;
        Ok(self.next_view_id)
    }

    #[cfg(test)]
    fn set_next_view_id_for_test(&mut self, value: u64) {
        self.next_view_id = value;
    }

    #[cfg(test)]
    pub(crate) fn inject_crash_at(&mut self, point: ProviderSessionCrashPoint) {
        self.crash_point = Some(point);
    }

    #[cfg(test)]
    fn crash_if(&mut self, point: ProviderSessionCrashPoint) {
        if self.crash_point == Some(point) {
            self.crash_point = None;
            panic!("injected provider-session crash at {point:?}");
        }
    }
}

impl<L: ProviderProcessLauncher, S: ProviderSessionStateStore> Drop
    for ProviderSessionManager<L, S>
{
    fn drop(&mut self) {
        // A manager may disappear during a crash or executor cancellation.
        // Transfer every still-owned root to the injected process-registry
        // recovery owner and journal UnknownLeaked. Process-local singleton
        // state is not a recovery authority: restart authority belongs to the
        // durable store plus the managed-process bridge.
        let slots = std::mem::take(&mut self.leases);
        for (agent_id, slot) in slots {
            let LeaseSlot { lease, mut state } = slot;
            self.pending_launches.remove(&agent_id);
            let closed_journal_has_leftover_lease = self
                .current
                .get(&agent_id)
                .is_some_and(|runtime| runtime.lifecycle() == RuntimeLifecycle::Closed);
            // A crash after the Closed journal but before lease removal must
            // retain Closed as the recovery key; rewriting it to UnknownLeaked
            // would hide the idempotent closed-leftover reconciliation path.
            if !closed_journal_has_leftover_lease {
                state.lifecycle = PersistedRuntimeLifecycle::UnknownLeaked;
            }
            state.revision = 0;
            let revision = self.next_state_revision.entry(agent_id).or_insert(0);
            if let Some(next_revision) = revision.checked_add(1) {
                *revision = next_revision;
                state.revision = next_revision;
                let _ = self.state_store.persist(state.clone());
            }
            let _ = self.launcher.retain_for_recovery(&state, lease);
        }
        for (agent_id, mut state) in std::mem::take(&mut self.pending_launches) {
            state.lifecycle = PersistedRuntimeLifecycle::UnknownLeaked;
            state.revision = 0;
            let revision = self.next_state_revision.entry(agent_id).or_insert(0);
            if let Some(next_revision) = revision.checked_add(1) {
                *revision = next_revision;
                state.revision = next_revision;
                let _ = self.state_store.persist(state);
            }
        }
    }
}

fn validate_lease(
    request: &ProviderRuntimeLaunchRequest,
    lease: &ProviderProcessLease,
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
    provider_session_id: Option<ProviderSessionId>,
) -> Result<ProviderLaunchMode, ProviderSessionError> {
    if request.agent.provider_kind != request.launch_spec.capabilities.kind {
        return Err(ProviderSessionError::ProviderKindMismatch {
            agent: request.agent.provider_kind,
            capabilities: request.launch_spec.capabilities.kind,
        });
    }
    match request.mode {
        ProviderSessionStartMode::NewConversation => Ok(ProviderLaunchMode::NewConversation),
        ProviderSessionStartMode::Open | ProviderSessionStartMode::ResumeExact => {
            let Some(provider_session_id) = provider_session_id else {
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
            if request.launch_spec.capabilities.exact_resume != CapabilitySupport::Supported {
                return Err(ProviderSessionError::ExactResumeUnavailable {
                    provider: request.agent.provider_kind,
                });
            }
            Ok(ProviderLaunchMode::ResumeExact(provider_session_id))
        }
    }
}

fn provider_session_id_for_request(
    request: &StartProviderSessionRequest,
    persisted: Option<&ProviderSessionState>,
) -> Result<Option<ProviderSessionId>, ProviderSessionError> {
    let persisted_id = persisted.and_then(ProviderSessionState::provider_session_id);
    if !matches!(request.mode, ProviderSessionStartMode::NewConversation) {
        if let (Some(existing), Some(received)) = (
            persisted_id.as_ref(),
            request.agent.provider_session_id.as_ref(),
        ) {
            if existing != received {
                return Err(ProviderSessionError::ProviderSessionIdRebind {
                    existing: existing.clone(),
                    received: received.clone(),
                });
            }
        }
    }
    Ok(request.agent.provider_session_id.clone().or(persisted_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::identity::ManagedProcessIdentity;
    use crate::process::registry::{JoinedActiveProcessZeroProof, ProviderManagedProcessPermit};
    use static_assertions::assert_not_impl_any;

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
        let provenance = ProviderSessionStartProvenance::mint(correlation, id.clone());
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
            cwd: PathBuf::from("."),
            environment: BTreeMap::new(),
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
            task_id,
            resource_id: ResourceId::new(),
            terminal_id: TerminalId::new(),
            generation: 1,
            launch_nonce: correlation.launch_nonce,
        };
        let request = ProviderRuntimeLaunchRequest {
            correlation,
            launch_spec,
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
        let first = ProviderSessionStartProvenance::mint(correlation, first_id.clone());
        assert_eq!(
            runtime.accept_provider_session_start(&first).unwrap(),
            ProviderIdentityAcceptance::Accepted
        );
        assert_eq!(
            runtime.accept_provider_session_start(&first).unwrap(),
            ProviderIdentityAcceptance::AlreadyAccepted
        );
        let rebind = ProviderSessionStartProvenance::mint(correlation, second_id);
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
        let wrong_nonce = ProviderSessionStartProvenance::mint(
            runtime
                .correlation()
                .set_launch_nonce_for_test(LaunchNonce::new()),
            id.clone(),
        );
        assert!(matches!(
            runtime.accept_provider_session_start(&wrong_nonce),
            Err(ProviderSessionError::WrongLaunchNonce)
        ));
        let stale = ProviderSessionStartProvenance::mint(
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
        let _ = id;
    }

    #[test]
    fn view_id_exhaustion_is_checked_instead_of_wrapping() {
        let launcher = FixtureProviderProcessLauncher::new();
        let mut manager = ProviderSessionManager::new(launcher);
        let task_id = TaskId::new();
        let agent =
            AgentSessionFacts::new(task_id, AgentRole::Primary, ProviderKind::ClaudeCode, None)
                .unwrap();
        let executable = ProviderExecutable::from_path(std::env::current_exe().unwrap()).unwrap();
        let capabilities = ProviderCapabilities {
            kind: ProviderKind::ClaudeCode,
            version: crate::providers::capabilities::ProviderVersion::new("fixture").unwrap(),
            auth_state: crate::providers::capabilities::ProviderAuthState::Unknown,
            exact_resume: CapabilitySupport::Supported,
            semantic_events: CapabilitySupport::Supported,
            provider_session_id: CapabilitySupport::Supported,
            build_launch: CapabilitySupport::Supported,
            parse_signal: CapabilitySupport::Supported,
            cooperative_stop: CapabilitySupport::Supported,
            observe_quota: CapabilitySupport::Unknown,
            evidence: Vec::new(),
        };
        let request = StartProviderSessionRequest::with_test_launch_spec(
            agent,
            ProviderAdapterLaunchSpec::from_test(
                executable,
                Vec::new(),
                PathBuf::from("."),
                BTreeMap::new(),
                capabilities,
            )
            .unwrap(),
            ProviderSessionStartMode::NewConversation,
        );
        let runtime = manager.start(request).unwrap();
        manager.set_next_view_id_for_test(u64::MAX);
        assert!(matches!(
            manager.attach_terminal_view(runtime.correlation()),
            Err(ProviderSessionError::ViewIdExhausted)
        ));
    }

    #[test]
    fn views_share_one_generation_and_root_exit_does_not_release_the_lease() {
        let launcher = FixtureProviderProcessLauncher::new();
        let mut manager = ProviderSessionManager::new(launcher.clone());
        let agent = AgentSessionFacts::new(
            TaskId::new(),
            AgentRole::Primary,
            ProviderKind::ClaudeCode,
            None,
        )
        .unwrap();
        let runtime = manager.start(test_request(agent.clone())).unwrap();
        let terminal = manager.attach_terminal_view(runtime.correlation()).unwrap();
        let semantic = manager.subscribe_semantic(runtime.correlation()).unwrap();
        assert_eq!(terminal.correlation(), semantic.correlation());
        assert_eq!(terminal.process_id(), semantic.process_id());
        assert_eq!(terminal.generation(), runtime.generation());
        drop(terminal);
        assert!(manager.attach_terminal_view(runtime.correlation()).is_ok());
        manager.process_exited(runtime.correlation()).unwrap();
        assert!(runtime.root_exit_observed());
        assert!(launcher.snapshot().stopped().is_empty());
        drop(semantic);
        manager.close_agent_session(agent.id).unwrap();
        assert_eq!(launcher.snapshot().stopped().len(), 1);
    }

    #[test]
    fn exact_resume_failure_is_visible_without_fresh_fallback() {
        let provider_session_id = ProviderSessionId::new("missing-provider-session").unwrap();
        let agent = AgentSessionFacts::new(
            TaskId::new(),
            AgentRole::Primary,
            ProviderKind::ClaudeCode,
            Some(provider_session_id.clone()),
        )
        .unwrap();
        let launcher = FixtureProviderProcessLauncher::new();
        launcher.fail_next(ProviderLaunchError::ExactResumeFailed(
            ExactResumeFailure::NotFound,
        ));
        let mut manager = ProviderSessionManager::new(launcher.clone());
        let mut request = test_request(agent);
        request.mode = ProviderSessionStartMode::Open;
        request.launch_proof = Some(
            ProviderAdapterLaunchProof::from_test(
                request.launch_spec.clone(),
                ProviderSessionStartMode::Open,
                Some(provider_session_id.clone()),
            )
            .unwrap(),
        );
        assert!(matches!(
            manager.start(request),
            Err(ProviderSessionError::ExactResumeFailed {
                provider_session_id: received,
                failure: ExactResumeFailure::NotFound,
            }) if received == provider_session_id
        ));
        assert_eq!(launcher.snapshot().launches().len(), 1);
    }

    #[test]
    fn unsupported_exact_resume_is_rejected_before_any_launch_effect() {
        let provider_session_id = ProviderSessionId::new("exact-resume").unwrap();
        let agent = AgentSessionFacts::new(
            TaskId::new(),
            AgentRole::Primary,
            ProviderKind::ClaudeCode,
            Some(provider_session_id),
        )
        .unwrap();
        let mut capabilities = test_capabilities();
        capabilities.exact_resume = CapabilitySupport::Unsupported;
        let spec = ProviderAdapterLaunchSpec::from_test(
            ProviderExecutable::from_path(std::env::current_exe().unwrap()).unwrap(),
            vec![OsString::from("--fixture")],
            PathBuf::from("."),
            BTreeMap::new(),
            capabilities,
        )
        .unwrap();
        let request = StartProviderSessionRequest::with_test_launch_spec(
            agent,
            spec,
            ProviderSessionStartMode::ResumeExact,
        );
        let launcher = FixtureProviderProcessLauncher::new();
        let mut manager = ProviderSessionManager::new(launcher.clone());
        assert!(matches!(
            manager.start(request),
            Err(ProviderSessionError::ExactResumeUnavailable { .. })
        ));
        assert!(launcher.snapshot().launches().is_empty());
    }

    #[test]
    fn persisted_provider_session_id_rejects_a_different_resume_request_before_teardown() {
        let launcher = FixtureProviderProcessLauncher::new();
        let issuer = FixtureProviderSessionStartIssuer::default();
        let agent = AgentSessionFacts::new(
            TaskId::new(),
            AgentRole::Primary,
            ProviderKind::ClaudeCode,
            None,
        )
        .unwrap();
        let mut manager = ProviderSessionManager::new(launcher.clone());
        let runtime = manager.start(test_request(agent.clone())).unwrap();
        let persisted_id = ProviderSessionId::new("persisted-session").unwrap();
        manager
            .accept_provider_session_start(
                issuer.issue(runtime.correlation(), persisted_id.clone()),
            )
            .unwrap();

        let mut mismatched_agent = agent;
        mismatched_agent.id = runtime.agent_session_id();
        mismatched_agent.task_id = runtime.task_id();
        mismatched_agent.runtime_generation = runtime.generation();
        mismatched_agent.provider_session_id =
            Some(ProviderSessionId::new("different-session").unwrap());
        let spec = ProviderAdapterLaunchSpec::from_test(
            ProviderExecutable::from_path(std::env::current_exe().unwrap()).unwrap(),
            vec![OsString::from("--fixture")],
            PathBuf::from("."),
            BTreeMap::new(),
            test_capabilities(),
        )
        .unwrap();
        let request = StartProviderSessionRequest::with_test_launch_spec(
            mismatched_agent,
            spec,
            ProviderSessionStartMode::Open,
        );
        assert!(matches!(
            manager.replace_generation(request),
            Err(ProviderSessionError::ProviderSessionIdRebind {
                existing,
                received
            }) if existing == persisted_id && received.as_str() == "different-session"
        ));
        assert!(launcher.snapshot().stopped().is_empty());
    }

    fn test_capabilities() -> ProviderCapabilities {
        ProviderCapabilities {
            kind: ProviderKind::ClaudeCode,
            version: crate::providers::capabilities::ProviderVersion::new("fixture").unwrap(),
            auth_state: crate::providers::capabilities::ProviderAuthState::Unknown,
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

    fn test_request(agent: AgentSessionFacts) -> StartProviderSessionRequest {
        let spec = ProviderAdapterLaunchSpec::from_test(
            ProviderExecutable::from_path(std::env::current_exe().unwrap()).unwrap(),
            vec![OsString::from("--fixture")],
            PathBuf::from("."),
            BTreeMap::new(),
            test_capabilities(),
        )
        .unwrap();
        StartProviderSessionRequest::with_test_launch_spec(
            agent,
            spec,
            ProviderSessionStartMode::NewConversation,
        )
    }

    #[test]
    fn crash_after_starting_journal_reopens_as_unknown_leaked() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let agent = AgentSessionFacts::new(
            TaskId::new(),
            AgentRole::Primary,
            ProviderKind::ClaudeCode,
            None,
        )
        .unwrap();
        let path = tempfile::NamedTempFile::new().unwrap();
        let store = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        let mut manager =
            ProviderSessionManager::with_state_store(FixtureProviderProcessLauncher::new(), store);
        manager.inject_crash_at(ProviderSessionCrashPoint::AfterStartingJournal);
        let result = catch_unwind(AssertUnwindSafe(|| {
            manager.start(test_request(agent.clone()))
        }));
        assert!(result.is_err());
        drop(manager);

        let reopened = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        let state = reopened.load(agent.id).unwrap().unwrap();
        assert_eq!(state.lifecycle(), PersistedRuntimeLifecycle::UnknownLeaked);
    }

    #[test]
    fn crash_after_permit_acquisition_reopens_as_unknown_leaked() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let agent = AgentSessionFacts::new(
            TaskId::new(),
            AgentRole::Primary,
            ProviderKind::ClaudeCode,
            None,
        )
        .unwrap();
        let path = tempfile::NamedTempFile::new().unwrap();
        let store = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        let mut manager =
            ProviderSessionManager::with_state_store(FixtureProviderProcessLauncher::new(), store);
        manager.inject_crash_at(ProviderSessionCrashPoint::AfterPermitAcquired);
        let result = catch_unwind(AssertUnwindSafe(|| {
            manager.start(test_request(agent.clone()))
        }));
        assert!(result.is_err());
        drop(manager);

        let reopened = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        let state = reopened.load(agent.id).unwrap().unwrap();
        assert_eq!(state.lifecycle(), PersistedRuntimeLifecycle::UnknownLeaked);
    }

    #[test]
    fn crash_before_stopping_journal_retains_recovery_requirement() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let agent = AgentSessionFacts::new(
            TaskId::new(),
            AgentRole::Primary,
            ProviderKind::ClaudeCode,
            None,
        )
        .unwrap();
        let path = tempfile::NamedTempFile::new().unwrap();
        let store = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        let mut manager =
            ProviderSessionManager::with_state_store(FixtureProviderProcessLauncher::new(), store);
        let runtime = manager.start(test_request(agent.clone())).unwrap();
        manager.inject_crash_at(ProviderSessionCrashPoint::BeforeStoppingJournal);
        let result = catch_unwind(AssertUnwindSafe(|| manager.close_agent_session(agent.id)));
        assert!(result.is_err());
        drop(runtime);
        drop(manager);

        let reopened = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        let state = reopened.load(agent.id).unwrap().unwrap();
        assert_eq!(state.lifecycle(), PersistedRuntimeLifecycle::UnknownLeaked);
    }

    #[test]
    fn crash_after_joined_zero_reopens_as_unknown_until_recovery_reconciles() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let agent = AgentSessionFacts::new(
            TaskId::new(),
            AgentRole::Primary,
            ProviderKind::ClaudeCode,
            None,
        )
        .unwrap();
        let path = tempfile::NamedTempFile::new().unwrap();
        let store = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        let mut manager =
            ProviderSessionManager::with_state_store(FixtureProviderProcessLauncher::new(), store);
        let runtime = manager.start(test_request(agent.clone())).unwrap();
        manager.inject_crash_at(ProviderSessionCrashPoint::AfterJoinedZero);
        let result = catch_unwind(AssertUnwindSafe(|| manager.close_agent_session(agent.id)));
        assert!(result.is_err());
        drop(runtime);
        drop(manager);

        let reopened = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        let state = reopened.load(agent.id).unwrap().unwrap();
        assert_eq!(state.lifecycle(), PersistedRuntimeLifecycle::UnknownLeaked);
    }

    #[test]
    fn closed_state_reconciles_a_leftover_recovery_lease_idempotently() {
        let agent = AgentSessionFacts::new(
            TaskId::new(),
            AgentRole::Primary,
            ProviderKind::ClaudeCode,
            None,
        )
        .unwrap();
        let path = tempfile::NamedTempFile::new().unwrap();
        let launcher = FixtureProviderProcessLauncher::new();
        let mut manager = ProviderSessionManager::with_state_store(
            launcher.clone(),
            SqliteProviderSessionStateStore::open(path.path()).unwrap(),
        );
        let runtime = manager.start(test_request(agent.clone())).unwrap();
        drop(runtime);
        drop(manager);

        let mut store = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        let mut closed = store.load(agent.id).unwrap().unwrap();
        closed.lifecycle = PersistedRuntimeLifecycle::Closed;
        closed.revision += 1;
        store.persist(closed).unwrap();
        drop(store);

        let mut reopened = ProviderSessionManager::with_state_store(
            launcher.clone(),
            SqliteProviderSessionStateStore::open(path.path()).unwrap(),
        );
        reopened.close_task(agent.task_id).unwrap();
        reopened.close_task(agent.task_id).unwrap();
        drop(reopened);

        assert_eq!(launcher.snapshot().stopped().len(), 1);
        let final_store = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        assert_eq!(
            final_store.load(agent.id).unwrap().unwrap().lifecycle(),
            PersistedRuntimeLifecycle::Closed
        );
    }

    #[test]
    fn crash_after_closed_journal_reconciles_current_leftover_lease() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let agent = AgentSessionFacts::new(
            TaskId::new(),
            AgentRole::Primary,
            ProviderKind::ClaudeCode,
            None,
        )
        .unwrap();
        let launcher = FixtureProviderProcessLauncher::new();
        let mut manager = ProviderSessionManager::new(launcher.clone());
        let runtime = manager.start(test_request(agent.clone())).unwrap();
        manager.inject_crash_at(ProviderSessionCrashPoint::AfterClosedJournal);
        let result = catch_unwind(AssertUnwindSafe(|| manager.close_agent_session(agent.id)));
        assert!(result.is_err());
        assert_eq!(runtime.lifecycle(), RuntimeLifecycle::Closed);

        manager.close_agent_session(agent.id).unwrap();
        drop(runtime);
        drop(manager);
        // The first stop/join completed before the crash; the retry is
        // idempotent and does not issue a second stop for the same process.
        assert_eq!(launcher.snapshot().stopped().len(), 1);
    }

    #[test]
    fn drop_after_closed_journal_preserves_durable_closed_recovery_key() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let agent = AgentSessionFacts::new(
            TaskId::new(),
            AgentRole::Primary,
            ProviderKind::ClaudeCode,
            None,
        )
        .unwrap();
        let path = tempfile::NamedTempFile::new().unwrap();
        let launcher = FixtureProviderProcessLauncher::new();
        {
            let mut manager = ProviderSessionManager::with_state_store(
                launcher.clone(),
                SqliteProviderSessionStateStore::open(path.path()).unwrap(),
            );
            let runtime = manager.start(test_request(agent.clone())).unwrap();
            manager.inject_crash_at(ProviderSessionCrashPoint::AfterClosedJournal);
            let result = catch_unwind(AssertUnwindSafe(|| manager.close_agent_session(agent.id)));
            assert!(result.is_err());
            assert_eq!(runtime.lifecycle(), RuntimeLifecycle::Closed);
        }

        let mut reopened = ProviderSessionManager::with_state_store(
            launcher.clone(),
            SqliteProviderSessionStateStore::open(path.path()).unwrap(),
        );
        reopened.close_task(agent.task_id).unwrap();
        assert_eq!(launcher.snapshot().stopped().len(), 1);
        assert_eq!(
            SqliteProviderSessionStateStore::open(path.path())
                .unwrap()
                .load(agent.id)
                .unwrap()
                .unwrap()
                .lifecycle(),
            PersistedRuntimeLifecycle::Closed
        );
    }

    #[test]
    fn process_authorities_are_opaque_nonclone_capabilities() {
        assert_not_impl_any!(ProviderManagedProcessPermit: Clone);
        assert_not_impl_any!(JoinedActiveProcessZeroProof: Clone);
    }

    #[test]
    fn sqlite_journal_reopens_exact_launch_state() {
        let runtime = unit_runtime();
        let path = tempfile::NamedTempFile::new().unwrap();
        let provider_session_id = ProviderSessionId::new("persisted-provider-session").unwrap();
        let state = ProviderSessionState {
            agent_session_id: runtime.agent_session_id(),
            task_id: runtime.task_id(),
            generation: runtime.generation(),
            revision: 1,
            lifecycle: PersistedRuntimeLifecycle::Starting,
            launch_nonce: runtime.launch_nonce(),
            launch_spec: runtime.launch_spec(),
            provider_session_id: Some(provider_session_id.clone()),
        };
        let mut first = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        first.persist(state.clone()).unwrap();
        drop(first);
        let second = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        let reopened = second.load(state.agent_session_id()).unwrap().unwrap();
        assert_eq!(reopened, state);
        assert_eq!(reopened.lifecycle(), PersistedRuntimeLifecycle::Starting);
        assert_eq!(reopened.launch_spec().arguments().count(), 0);
        assert_eq!(reopened.provider_session_id(), Some(provider_session_id));
    }

    #[test]
    fn sqlite_task_enumeration_separates_open_and_closed_recovery_states() {
        let runtime = unit_runtime();
        let task_id = runtime.task_id();
        let launch_spec = runtime.launch_spec();
        let open_id = runtime.agent_session_id();
        let closed_id = AgentSessionId::new();
        let failed_id = AgentSessionId::new();
        let state = |agent_session_id, lifecycle| ProviderSessionState {
            agent_session_id,
            task_id,
            generation: runtime.generation(),
            revision: 1,
            lifecycle,
            launch_nonce: runtime.launch_nonce(),
            launch_spec: launch_spec.clone(),
            provider_session_id: None,
        };
        let path = tempfile::NamedTempFile::new().unwrap();
        let mut store = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        store
            .persist(state(open_id, PersistedRuntimeLifecycle::Starting))
            .unwrap();
        store
            .persist(state(closed_id, PersistedRuntimeLifecycle::Closed))
            .unwrap();
        store
            .persist(state(failed_id, PersistedRuntimeLifecycle::LaunchFailed))
            .unwrap();

        let open = store.list_open_for_task(task_id).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].agent_session_id(), open_id);
        let recovery = store.list_recovery_for_task(task_id).unwrap();
        assert_eq!(recovery.len(), 1);
        assert_eq!(recovery[0].agent_session_id(), closed_id);
    }

    #[test]
    fn sqlite_session_start_token_is_one_shot_across_reopen() {
        let runtime = unit_runtime();
        let token = ProviderSessionStartProvenance::mint(
            runtime.correlation(),
            ProviderSessionId::new("durable-provider-session").unwrap(),
        );
        let path = tempfile::NamedTempFile::new().unwrap();
        let mut first = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        assert!(first.consume_session_start_token(&token).unwrap());
        drop(first);
        let mut second = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        assert!(!second.consume_session_start_token(&token).unwrap());
    }

    #[test]
    fn sqlite_session_start_token_rejects_rebind_with_the_same_token_id() {
        let runtime = unit_runtime();
        let first_id = ProviderSessionId::new("durable-provider-session").unwrap();
        let second_id = ProviderSessionId::new("different-provider-session").unwrap();
        let first = ProviderSessionStartProvenance::mint(runtime.correlation(), first_id);
        let mismatched = ProviderSessionStartProvenance {
            correlation: runtime.correlation(),
            provider_session_id: second_id,
            token_id: first.token_id(),
        };
        let path = tempfile::NamedTempFile::new().unwrap();
        let mut store = SqliteProviderSessionStateStore::open(path.path()).unwrap();
        assert!(store.consume_session_start_token(&first).unwrap());
        assert!(store
            .consume_session_start_token(&mismatched)
            .unwrap_err()
            .contains("provenance mismatch"));
    }

    #[test]
    fn sqlite_session_start_token_has_one_winner_across_connections() {
        let runtime = unit_runtime();
        let correlation = runtime.correlation();
        let provider_session_id = ProviderSessionId::new("concurrent-provider-session").unwrap();
        let token_id = Uuid::now_v7();
        let path = tempfile::NamedTempFile::new().unwrap();
        let path = path.path().to_path_buf();
        // Initialize the schema before the workers race; the concurrency
        // boundary under test is the token transaction, not DDL setup.
        drop(SqliteProviderSessionStateStore::open(&path).unwrap());
        let first_store = SqliteProviderSessionStateStore::open(&path).unwrap();
        let second_store = SqliteProviderSessionStateStore::open(&path).unwrap();
        let mut workers = Vec::new();
        for store in [first_store, second_store] {
            let provider_session_id = provider_session_id.clone();
            workers.push(std::thread::spawn(move || {
                let mut store = store;
                let provenance = ProviderSessionStartProvenance {
                    correlation,
                    provider_session_id,
                    token_id,
                };
                store.consume_session_start_token(&provenance)
            }));
        }
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|accepted| **accepted).count(), 1);
        assert_eq!(results.iter().filter(|accepted| !**accepted).count(), 1);
    }
}
