//! Generation-fenced ownership of managed process roots.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::domain::id::ResourceId;
use crate::domain::operation::ResourceFence;
use crate::kernel::{RuntimeRegistry, RuntimeRegistryError};
use crate::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
use crate::process::teardown::{TeardownReleaseAuthority, TeardownTicket};

pub const MAX_PROCESS_DISPLAY_LABEL_BYTES: usize = 256;

mod sealed {
    pub(crate) trait JobMembership {}

    #[cfg(test)]
    impl<T> JobMembership for T {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessDisplayLabel(String);

impl ProcessDisplayLabel {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ProcessDisplayLabelError> {
        let value = value.as_ref();
        if value.trim().is_empty() {
            return Err(ProcessDisplayLabelError::Empty);
        }
        if value.len() > MAX_PROCESS_DISPLAY_LABEL_BYTES {
            return Err(ProcessDisplayLabelError::TooLong {
                actual: value.len(),
                max: MAX_PROCESS_DISPLAY_LABEL_BYTES,
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessDisplayLabelError {
    Empty,
    TooLong { actual: usize, max: usize },
}

impl fmt::Display for ProcessDisplayLabelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "managed process display label must be non-empty"),
            Self::TooLong { actual, max } => write!(
                f,
                "managed process display label is {actual} bytes; maximum is {max}"
            ),
        }
    }
}

impl std::error::Error for ProcessDisplayLabelError {}

/// Read-only seam for asking the operating-system Job which processes it owns.
///
/// This query proves current membership only. It is deliberately not evidence
/// that a caller successfully assigned a process to the Job.
///
/// The seam is crate-private: exposing it would let arbitrary callers block
/// registry release or fabricate the membership observations that protect the
/// receiver-owned ACTIVE_PROCESS_ZERO proof. Production entries use the
/// concrete managed Windows Job adapter below.
///
/// The concrete adapter's membership queries and listener release are bounded
/// by the Job/IOCP polling contract. An external blocking implementation is
/// not an allowed registry authority.
///
/// ```compile_fail
/// use devmanager::process::registry::{JobMemberInfo, JobMembership};
///
/// struct ExternalMembership;
///
/// impl JobMembership for ExternalMembership {
///     fn active_process_ids(&self) -> Result<Vec<u32>, String> {
///         Ok(Vec::new())
///     }
///     fn inspect_process(&self, _pid: u32) -> Result<JobMemberInfo, String> {
///         panic!("external implementation must be rejected")
///     }
/// }
/// ```
#[allow(dead_code)]
pub(crate) trait JobMembership: sealed::JobMembership {
    fn active_process_ids(&self) -> Result<Vec<u32>, String>;

    fn active_process_ids_until(&self, absolute_deadline: Instant) -> Result<Vec<u32>, String> {
        if Instant::now() >= absolute_deadline {
            return Err("Job membership query exceeded teardown absolute deadline".to_string());
        }
        let process_ids = self.active_process_ids()?;
        if Instant::now() >= absolute_deadline {
            return Err("Job membership query exceeded teardown absolute deadline".to_string());
        }
        Ok(process_ids)
    }

    /// Terminate the owned Job tree, never a PID-selected process.
    fn terminate_tree(&self) -> Result<(), String> {
        Err("Job tree termination is unavailable for this membership".to_string())
    }

    fn inspect_process(&self, pid: u32) -> Result<JobMemberInfo, String> {
        Err(format!("process identity for PID {pid} is inaccessible"))
    }

    fn inspect_process_until(
        &self,
        pid: u32,
        absolute_deadline: Instant,
    ) -> Result<JobMemberInfo, String> {
        if Instant::now() >= absolute_deadline {
            return Err("Job member inspection exceeded teardown absolute deadline".to_string());
        }
        let member = self.inspect_process(pid)?;
        if Instant::now() >= absolute_deadline {
            return Err("Job member inspection exceeded teardown absolute deadline".to_string());
        }
        Ok(member)
    }

    fn bind_completion_fence(&mut self, _fence: ManagedProcessFence) -> Result<(), String> {
        Ok(())
    }

    /// Stops receiver-owned listeners before a stopped registry entry is
    /// removed. Implementations that do not own a listener may accept the
    /// default no-op.
    fn shutdown_for_release(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn shutdown_for_release_until(&mut self, _absolute_deadline: Instant) -> Result<(), String> {
        self.shutdown_for_release()
    }

    fn drain_completion_messages_until(
        &self,
        absolute_deadline: Instant,
    ) -> Result<Vec<JobCompletionMessage>, String> {
        if Instant::now() >= absolute_deadline {
            return Err("Job completion drain exceeded teardown absolute deadline".to_string());
        }
        Ok(Vec::new())
    }
}

#[cfg(not(test))]
impl sealed::JobMembership for crate::process::job::ManagedProcessJob {}

impl JobMembership for crate::process::job::ManagedProcessJob {
    fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        crate::process::job::ManagedProcessJob::active_process_ids(self)
    }

    fn active_process_ids_until(&self, absolute_deadline: Instant) -> Result<Vec<u32>, String> {
        crate::process::job::ManagedProcessJob::active_process_ids_until(self, absolute_deadline)
    }

    fn terminate_tree(&self) -> Result<(), String> {
        crate::process::job::ManagedProcessJob::terminate_tree(self)
    }

    fn inspect_process(&self, pid: u32) -> Result<JobMemberInfo, String> {
        crate::process::job::ManagedProcessJob::inspect_process(self, pid)
    }

    fn inspect_process_until(
        &self,
        pid: u32,
        absolute_deadline: Instant,
    ) -> Result<JobMemberInfo, String> {
        crate::process::job::ManagedProcessJob::inspect_process_until(self, pid, absolute_deadline)
    }

    fn bind_completion_fence(&mut self, fence: ManagedProcessFence) -> Result<(), String> {
        crate::process::job::ManagedProcessJob::bind_completion_fence(self, fence)
    }

    fn shutdown_for_release(&mut self) -> Result<(), String> {
        crate::process::job::ManagedProcessJob::shutdown_for_release(self)
    }

    fn shutdown_for_release_until(&mut self, absolute_deadline: Instant) -> Result<(), String> {
        crate::process::job::ManagedProcessJob::shutdown_for_release_until(self, absolute_deadline)
    }

    fn drain_completion_messages_until(
        &self,
        absolute_deadline: Instant,
    ) -> Result<Vec<JobCompletionMessage>, String> {
        crate::process::job::ManagedProcessJob::drain_completion_messages_until(
            self,
            absolute_deadline,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManagedProcessState {
    Starting,
    Running,
    Stopping,
    /// The owning Job has authoritatively reported zero active processes, but
    /// its completion receiver and registry entry are still retained.  This
    /// is deliberately not named `Stopped`: externally visible stopped state
    /// may be published only after exact Job release removes the live entry.
    ZeroSettled,
    Failed,
    Leaked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobMemberInfo {
    identity: ManagedProcessIdentity,
    command_line: Option<String>,
}

impl JobMemberInfo {
    pub fn new(identity: ManagedProcessIdentity, command_line: Option<String>) -> Self {
        Self {
            identity,
            command_line,
        }
    }

    pub fn identity(&self) -> &ManagedProcessIdentity {
        &self.identity
    }

    pub fn command_line(&self) -> Option<&str> {
        self.command_line.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobCompletionEvent {
    NewProcess { pid: u32 },
    ExitProcess { pid: u32 },
    AbnormalExitProcess { pid: u32 },
    ActiveProcessZero,
    Limit { message_id: u32, pid: Option<u32> },
    MonitorFailed { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCompletionMessage {
    fence: ManagedProcessFence,
    event: JobCompletionEvent,
}

impl JobCompletionMessage {
    pub(crate) fn from_completion_receiver(
        _receiver: &crate::process::job::CompletionReceiverToken,
        fence: ManagedProcessFence,
        event: JobCompletionEvent,
    ) -> Self {
        Self { fence, event }
    }

    pub fn fence(&self) -> &ManagedProcessFence {
        &self.fence
    }

    pub fn event(&self) -> &JobCompletionEvent {
        &self.event
    }
}

/// A caller-visible completion observation. It is intentionally distinct from
/// `JobCompletionMessage`, which is produced only by the concrete managed Job
/// completion receiver and is the sole input that can create zero authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCompletionObservation {
    fence: ManagedProcessFence,
    event: JobCompletionEvent,
}

impl JobCompletionObservation {
    pub fn new(fence: ManagedProcessFence, event: JobCompletionEvent) -> Self {
        Self { fence, event }
    }

    pub fn fence(&self) -> &ManagedProcessFence {
        &self.fence
    }

    pub fn event(&self) -> &JobCompletionEvent {
        &self.event
    }
}

/// A registry-issued receipt for one exact ACTIVE_PROCESS_ZERO completion.
///
/// The fence and nonce are private so teardown adapters cannot invent a zero
/// observation. The registry keeps the matching receipt pending until an
/// authoritative membership query consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveProcessZeroProof {
    fence: ManagedProcessFence,
    nonce: u64,
}

impl ActiveProcessZeroProof {
    fn from_completion(fence: ManagedProcessFence, nonce: u64) -> Self {
        Self { fence, nonce }
    }

    pub fn fence(&self) -> &ManagedProcessFence {
        &self.fence
    }
}

#[derive(Debug)]
pub struct RegisteredProcess<J> {
    fence: ResourceFence,
    owner: ProcessOwner,
    root: ManagedProcessIdentity,
    display_label: ProcessDisplayLabel,
    job: J,
    state: ManagedProcessState,
    known_members: Vec<JobMemberInfo>,
    unknown_member_pids: Vec<u32>,
    last_limit: Option<(u32, Option<u32>)>,
    pending_zero_prior_state: Option<ManagedProcessState>,
    pending_zero_proof: Option<ActiveProcessZeroProof>,
    authoritative_zero_settled: bool,
    settled_zero_nonce: Option<u64>,
    next_zero_proof_nonce: u64,
}

impl<J> RegisteredProcess<J> {
    pub(crate) fn new(
        fence: ResourceFence,
        owner: ProcessOwner,
        root: ManagedProcessIdentity,
        display_label: ProcessDisplayLabel,
        job: J,
    ) -> Self {
        Self {
            fence,
            owner,
            root,
            display_label,
            job,
            state: ManagedProcessState::Starting,
            known_members: Vec::new(),
            unknown_member_pids: Vec::new(),
            last_limit: None,
            pending_zero_prior_state: None,
            pending_zero_proof: None,
            authoritative_zero_settled: false,
            settled_zero_nonce: None,
            next_zero_proof_nonce: 1,
        }
    }

    pub fn fence(&self) -> ResourceFence {
        self.fence
    }

    pub fn owner(&self) -> ProcessOwner {
        self.owner
    }

    pub fn root(&self) -> &ManagedProcessIdentity {
        &self.root
    }

    pub fn display_label(&self) -> &str {
        self.display_label.as_str()
    }

    pub(crate) fn job(&self) -> &J {
        &self.job
    }

    pub fn state(&self) -> ManagedProcessState {
        self.state
    }

    pub fn known_members(&self) -> &[JobMemberInfo] {
        &self.known_members
    }

    pub fn unknown_member_pids(&self) -> &[u32] {
        &self.unknown_member_pids
    }

    pub fn member_count(&self) -> usize {
        self.known_members.len() + self.unknown_member_pids.len()
    }

    pub fn last_limit(&self) -> Option<(u32, Option<u32>)> {
        self.last_limit
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedProcessFence {
    resource: ResourceFence,
    owner: ProcessOwner,
    root: ManagedProcessIdentity,
}

impl ManagedProcessFence {
    /// Internal construction is reserved for the process registry and the
    /// future Task 3 managed-launch bridge. Provider callers receive an
    /// opaque permit instead of a caller-forged fence.
    pub(crate) fn new(
        resource: ResourceFence,
        owner: ProcessOwner,
        root: ManagedProcessIdentity,
    ) -> Self {
        Self {
            resource,
            owner,
            root,
        }
    }

    fn from_process<J>(process: &RegisteredProcess<J>) -> Self {
        Self::new(process.fence, process.owner, process.root.clone())
    }

    pub fn resource(&self) -> ResourceFence {
        self.resource
    }

    pub fn owner(&self) -> ProcessOwner {
        self.owner
    }

    pub fn root(&self) -> &ManagedProcessIdentity {
        &self.root
    }
}

/// Build an immutable membership snapshot for external contract tests.
///
/// This hidden seam carries only an exact identity observation into the port
/// authority projector. It does not expose a Job implementation, lifecycle
/// mutation, termination operation, or any other process ownership authority.
#[doc(hidden)]
pub fn test_managed_resource_snapshot(
    resource: ResourceFence,
    state: ManagedProcessState,
    root: ManagedProcessIdentity,
    members: Vec<ManagedProcessIdentity>,
    membership_revision: u64,
    observation_sequence: u64,
    observed_at: Instant,
    max_age: Duration,
    validity: crate::process::ports::ManagedProcessSnapshotValidity,
    detail: Option<String>,
) -> Result<crate::process::ports::ManagedResourceSnapshot, &'static str> {
    if membership_revision == 0 || observation_sequence == 0 {
        return Err("membership revision and observation sequence must be non-zero");
    }
    if validity == crate::process::ports::ManagedProcessSnapshotValidity::Valid
        && !members.iter().any(|member| member == &root)
    {
        return Err("valid membership snapshots must include the exact root identity");
    }

    let membership = match validity {
        crate::process::ports::ManagedProcessSnapshotValidity::Valid => {
            crate::process::ports::RegistryMembershipSnapshot::valid(
                membership_revision,
                observation_sequence,
                observed_at,
                max_age,
            )
        }
        crate::process::ports::ManagedProcessSnapshotValidity::Stale => {
            crate::process::ports::RegistryMembershipSnapshot::stale(
                membership_revision,
                observation_sequence,
                observed_at,
            )
        }
        crate::process::ports::ManagedProcessSnapshotValidity::Failed => {
            crate::process::ports::RegistryMembershipSnapshot::failed(
                membership_revision,
                observation_sequence,
                detail.unwrap_or_else(|| "membership observation failed".to_string()),
            )
        }
    };

    Ok(crate::process::ports::ManagedResourceSnapshot::new(
        ManagedProcessFence::new(resource, ProcessOwner::Host, root),
        state,
        members,
        membership,
    ))
}

/// Private ownership receipt embedded in a provider permit. The trait is
/// deliberately crate-private: an external caller can hold a permit returned
/// by the registry, but cannot implement or construct the authority seam.
/// There is intentionally no production implementation on this branch; the
/// Task 3 union must supply the real suspended Job-root/PTY receipt. Fixtures
/// implement it only under `cfg(test)`.
pub(crate) trait ProviderPermitOwnership: fmt::Debug + Send {}

/// Opaque, non-Clone authority for one exact process/PTY Job registration.
///
/// The value owns an authority receipt in addition to the generation fence.
/// Dropping or transferring the permit therefore remains visible to the
/// process-registry/Task 3 bridge rather than reducing ownership to an
/// inspectable PID or fence copy.
pub struct ProviderManagedProcessPermit {
    fence: ManagedProcessFence,
    #[allow(dead_code)]
    ownership: Box<dyn ProviderPermitOwnership>,
}

impl fmt::Debug for ProviderManagedProcessPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderManagedProcessPermit")
            .field("resource", &self.fence.resource())
            .field("owner", &self.fence.owner())
            .field("pid", &self.fence.root().id().pid())
            .finish()
    }
}

impl ProviderManagedProcessPermit {
    pub fn process_id(&self) -> ManagedProcessId {
        self.fence.root().id()
    }

    /// Registry/Task 3-only issuer. The private ownership trait makes this
    /// impossible to call from an external crate, even though the permit is
    /// part of the public launcher contract.
    #[allow(dead_code)]
    pub(crate) fn from_registry<T>(fence: ManagedProcessFence, ownership: T) -> Self
    where
        T: ProviderPermitOwnership + 'static,
    {
        Self {
            fence,
            ownership: Box::new(ownership),
        }
    }

    pub(crate) fn fence(&self) -> &ManagedProcessFence {
        &self.fence
    }
}

/// Opaque, non-Clone proof that the exact managed Job/PTY authority joined
/// ACTIVE_PROCESS_ZERO. There is intentionally no public status boolean or
/// fence constructor; failure to establish this value is represented by the
/// launcher error path.
pub struct JoinedActiveProcessZeroProof {
    fence: ManagedProcessFence,
    #[allow(dead_code)]
    receipt: Box<dyn ProviderPermitOwnership>,
}

impl fmt::Debug for JoinedActiveProcessZeroProof {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JoinedActiveProcessZeroProof")
            .field("resource", &self.fence.resource())
            .field("owner", &self.fence.owner())
            .finish()
    }
}

impl JoinedActiveProcessZeroProof {
    #[allow(dead_code)]
    pub(crate) fn from_registry<T>(fence: ManagedProcessFence, receipt: T) -> Self
    where
        T: ProviderPermitOwnership + 'static,
    {
        Self {
            fence,
            receipt: Box::new(receipt),
        }
    }

    pub(crate) fn matches_permit(&self, permit: &ProviderManagedProcessPermit) -> bool {
        self.fence == permit.fence
    }

    pub(crate) fn matches_fence(&self, fence: &ManagedProcessFence) -> bool {
        &self.fence == fence
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessClassification {
    Managed(ManagedProcessFence),
    External,
    ReconciliationFault {
        expected: ManagedProcessFence,
        observed: ManagedProcessIdentity,
        reason: OwnershipFault,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipFault {
    IdentityMismatch,
    NotJobMember,
    MembershipQueryFailed { detail: String },
}

#[derive(Debug)]
pub enum UnregisterOutcome<J> {
    Removed(RegisteredProcess<J>),
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessLifecycleOperation {
    CommitResumed,
    RollbackStarting,
    ReleaseStopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessRegistryError {
    ActiveGeneration {
        current: ResourceFence,
        proposed: ResourceFence,
    },
    DuplicateActivePid {
        pid: u32,
        existing_resource: ResourceId,
        proposed_resource: ResourceId,
    },
    NotJobMember {
        resource_id: ResourceId,
        pid: u32,
    },
    MembershipQueryFailed {
        resource_id: ResourceId,
        detail: String,
    },
    CompletionNotificationsFailed {
        resource_id: ResourceId,
        detail: String,
    },
    IdentityMismatch {
        resource_id: ResourceId,
        expected: ManagedProcessIdentity,
        observed: ManagedProcessIdentity,
    },
    ActiveProcessZeroUnproved {
        resource_id: ResourceId,
    },
    ReleaseAuthorityRequired {
        resource_id: ResourceId,
    },
    TeardownReleaseAuthorityMismatch {
        resource_id: ResourceId,
    },
    JobShutdownFailed {
        resource_id: ResourceId,
        detail: String,
    },
    InvalidLifecycleState {
        resource_id: ResourceId,
        operation: ProcessLifecycleOperation,
        state: ManagedProcessState,
    },
    StaleLifecycleFence {
        resource_id: ResourceId,
        operation: ProcessLifecycleOperation,
    },
    Runtime(RuntimeRegistryError),
}

impl fmt::Display for ProcessRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ActiveGeneration { current, proposed } => write!(
                f,
                "managed process resource {:?} is still active; cannot register {:?}",
                current, proposed
            ),
            Self::DuplicateActivePid {
                pid,
                existing_resource,
                proposed_resource,
            } => write!(
                f,
                "managed process PID {pid} belongs to resource {existing_resource}; resource {proposed_resource} cannot claim it"
            ),
            Self::NotJobMember { resource_id, pid } => write!(
                f,
                "managed process PID {pid} is not a member of resource {resource_id}'s Job"
            ),
            Self::MembershipQueryFailed {
                resource_id,
                detail,
            } => write!(
                f,
                "could not query resource {resource_id}'s Job membership: {detail}"
            ),
            Self::CompletionNotificationsFailed {
                resource_id,
                detail,
            } => write!(
                f,
                "could not start resource {resource_id}'s Job notifications: {detail}"
            ),
            Self::IdentityMismatch {
                resource_id,
                expected,
                observed,
            } => write!(
                f,
                "resource {resource_id} expected managed process identity {expected:?}, observed {observed:?}"
            ),
            Self::ActiveProcessZeroUnproved { resource_id } => write!(
                f,
                "resource {resource_id} has no matching ACTIVE_PROCESS_ZERO completion proof"
            ),
            Self::ReleaseAuthorityRequired { resource_id } => write!(
                f,
                "resource {resource_id} requires an opaque teardown release authority"
            ),
            Self::TeardownReleaseAuthorityMismatch { resource_id } => write!(
                f,
                "resource {resource_id} received a stale or mismatched teardown release authority"
            ),
            Self::JobShutdownFailed {
                resource_id,
                detail,
            } => write!(
                f,
                "resource {resource_id} could not stop its managed Job listener before release: {detail}"
            ),
            Self::InvalidLifecycleState {
                resource_id,
                operation,
                state,
            } => write!(
                f,
                "cannot perform {operation:?} for managed process {resource_id} while it is {state:?}"
            ),
            Self::StaleLifecycleFence {
                resource_id,
                operation,
            } => write!(
                f,
                "cannot perform {operation:?} for managed process {resource_id} through a stale fence"
            ),
            Self::Runtime(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ProcessRegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Runtime(error) => Some(error),
            Self::ActiveGeneration { .. }
            | Self::DuplicateActivePid { .. }
            | Self::NotJobMember { .. }
            | Self::MembershipQueryFailed { .. }
            | Self::CompletionNotificationsFailed { .. }
            | Self::IdentityMismatch { .. }
            | Self::ActiveProcessZeroUnproved { .. }
            | Self::ReleaseAuthorityRequired { .. }
            | Self::TeardownReleaseAuthorityMismatch { .. }
            | Self::JobShutdownFailed { .. }
            | Self::InvalidLifecycleState { .. }
            | Self::StaleLifecycleFence { .. } => None,
        }
    }
}

impl From<RuntimeRegistryError> for ProcessRegistryError {
    fn from(error: RuntimeRegistryError) -> Self {
        Self::Runtime(error)
    }
}

#[derive(Debug)]
pub struct ProcessRegistrationFailure<J> {
    reason: ProcessRegistryError,
    rejected: RegisteredProcess<J>,
}

impl<J> ProcessRegistrationFailure<J> {
    fn new(reason: ProcessRegistryError, rejected: RegisteredProcess<J>) -> Self {
        Self { reason, rejected }
    }

    pub fn reason(&self) -> &ProcessRegistryError {
        &self.reason
    }

    pub fn rejected(&self) -> &RegisteredProcess<J> {
        &self.rejected
    }

    pub fn into_parts(self) -> (ProcessRegistryError, RegisteredProcess<J>) {
        (self.reason, self.rejected)
    }
}

impl<J> fmt::Display for ProcessRegistrationFailure<J> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.reason.fmt(f)
    }
}

impl<J: fmt::Debug> std::error::Error for ProcessRegistrationFailure<J> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.reason)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CachedMembershipResult {
    Valid,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedMembershipObservation {
    fence: ManagedProcessFence,
    state: ManagedProcessState,
    members: Vec<ManagedProcessIdentity>,
    unknown_member_pids: Vec<u32>,
    result: CachedMembershipResult,
    membership_revision: u64,
    observation_sequence: u64,
}

#[derive(Debug)]
pub struct ProcessRegistry<J> {
    runtime: RuntimeRegistry,
    current: BTreeMap<ResourceId, RegisteredProcess<J>>,
    membership_revision: AtomicU64,
    observation_sequence: AtomicU64,
    membership_cache: Mutex<BTreeMap<ResourceId, CachedMembershipObservation>>,
}

impl<J> Default for ProcessRegistry<J> {
    fn default() -> Self {
        Self::new()
    }
}

impl<J> ProcessRegistry<J> {
    pub fn new() -> Self {
        Self {
            runtime: RuntimeRegistry::new(),
            current: BTreeMap::new(),
            membership_revision: AtomicU64::new(0),
            observation_sequence: AtomicU64::new(0),
            membership_cache: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn current(&self, resource_id: ResourceId) -> Option<&RegisteredProcess<J>> {
        self.current.get(&resource_id)
    }

    pub fn len(&self) -> usize {
        self.current.len()
    }

    /// Compare a full generation/owner/root fence against the current entry.
    /// PID or resource ID alone is never sufficient for teardown authority.
    pub fn exact_fence_matches(&self, fence: &ManagedProcessFence) -> bool {
        self.current
            .get(&fence.resource.resource_id)
            .is_some_and(|current| ManagedProcessFence::from_process(current) == *fence)
    }

    fn take_exact_in_state(
        &mut self,
        fence: &ManagedProcessFence,
        required_state: ManagedProcessState,
        operation: ProcessLifecycleOperation,
    ) -> Result<UnregisterOutcome<J>, ProcessRegistryError> {
        let resource_id = fence.resource.resource_id;
        let Some(current) = self.current.get(&resource_id) else {
            return Ok(UnregisterOutcome::Stale);
        };
        if ManagedProcessFence::from_process(current) != *fence {
            return Ok(UnregisterOutcome::Stale);
        }
        if current.state != required_state {
            return Err(ProcessRegistryError::InvalidLifecycleState {
                resource_id,
                operation,
                state: current.state,
            });
        }

        self.runtime.retire(fence.resource)?;
        let removed = self
            .current
            .remove(&resource_id)
            .expect("exact current registry entry was checked before removal");
        if let Ok(mut cache) = self.membership_cache.lock() {
            cache.remove(&resource_id);
        }
        Ok(UnregisterOutcome::Removed(removed))
    }

    pub fn rollback_starting_exact(
        &mut self,
        fence: &ManagedProcessFence,
    ) -> Result<UnregisterOutcome<J>, ProcessRegistryError> {
        self.take_exact_in_state(
            fence,
            ManagedProcessState::Starting,
            ProcessLifecycleOperation::RollbackStarting,
        )
    }

    pub fn release_stopped_exact(
        &mut self,
        fence: &ManagedProcessFence,
    ) -> Result<UnregisterOutcome<J>, ProcessRegistryError> {
        if let Some(current) = self.current.get(&fence.resource.resource_id) {
            if ManagedProcessFence::from_process(current) == *fence {
                return Err(ProcessRegistryError::ReleaseAuthorityRequired {
                    resource_id: fence.resource.resource_id,
                });
            }
            return Err(ProcessRegistryError::StaleLifecycleFence {
                resource_id: fence.resource.resource_id,
                operation: ProcessLifecycleOperation::ReleaseStopped,
            });
        }
        Err(ProcessRegistryError::StaleLifecycleFence {
            resource_id: fence.resource.resource_id,
            operation: ProcessLifecycleOperation::ReleaseStopped,
        })
    }

    /// Releases one stopped process only with the registry-minted authority
    /// bound to the exact teardown ticket, epoch, fence, and zero receipt.
    #[allow(private_bounds)]
    pub fn release_stopped_with_authority(
        &mut self,
        ticket: &TeardownTicket,
        authority: &TeardownReleaseAuthority,
    ) -> Result<UnregisterOutcome<J>, ProcessRegistryError>
    where
        J: JobMembership,
    {
        let absolute_deadline = Instant::now()
            .checked_add(Duration::from_secs(1))
            .ok_or_else(|| ProcessRegistryError::JobShutdownFailed {
                resource_id: ticket.resource_id(),
                detail: "managed Job release deadline overflow".to_string(),
            })?;
        self.release_stopped_with_authority_until(ticket, authority, absolute_deadline)
    }

    #[allow(private_bounds)]
    pub(crate) fn release_stopped_with_authority_until(
        &mut self,
        ticket: &TeardownTicket,
        authority: &TeardownReleaseAuthority,
        absolute_deadline: Instant,
    ) -> Result<UnregisterOutcome<J>, ProcessRegistryError>
    where
        J: JobMembership,
    {
        let resource_id = ticket.resource_id();
        let Some(current) = self.current.get_mut(&resource_id) else {
            return Err(ProcessRegistryError::StaleLifecycleFence {
                resource_id,
                operation: ProcessLifecycleOperation::ReleaseStopped,
            });
        };
        let current_fence = ManagedProcessFence::from_process(current);
        if current_fence != *ticket.fence() {
            return Err(ProcessRegistryError::StaleLifecycleFence {
                resource_id,
                operation: ProcessLifecycleOperation::ReleaseStopped,
            });
        }
        let Some(nonce) = current.settled_zero_nonce else {
            return Err(ProcessRegistryError::ReleaseAuthorityRequired { resource_id });
        };
        if !current.authoritative_zero_settled || !authority.matches(ticket, nonce) {
            return Err(ProcessRegistryError::TeardownReleaseAuthorityMismatch { resource_id });
        }
        if Instant::now() >= absolute_deadline {
            return Err(ProcessRegistryError::JobShutdownFailed {
                resource_id,
                detail: "managed Job release exceeded teardown absolute deadline".to_string(),
            });
        }
        if let Err(detail) = current.job.shutdown_for_release_until(absolute_deadline) {
            return Err(ProcessRegistryError::JobShutdownFailed {
                resource_id,
                detail,
            });
        }
        if Instant::now() >= absolute_deadline {
            return Err(ProcessRegistryError::JobShutdownFailed {
                resource_id,
                detail: "managed Job release exceeded teardown absolute deadline".to_string(),
            });
        }
        self.take_exact_in_state(
            ticket.fence(),
            ManagedProcessState::ZeroSettled,
            ProcessLifecycleOperation::ReleaseStopped,
        )
    }

    pub fn commit_resumed_exact(
        &mut self,
        fence: &ManagedProcessFence,
    ) -> Result<(), ProcessRegistryError> {
        let resource_id = fence.resource.resource_id;
        let Some(current) = self.current.get_mut(&resource_id) else {
            return Err(ProcessRegistryError::StaleLifecycleFence {
                resource_id,
                operation: ProcessLifecycleOperation::CommitResumed,
            });
        };
        if ManagedProcessFence::from_process(current) != *fence {
            return Err(ProcessRegistryError::StaleLifecycleFence {
                resource_id,
                operation: ProcessLifecycleOperation::CommitResumed,
            });
        }
        if current.state != ManagedProcessState::Starting {
            return Err(ProcessRegistryError::InvalidLifecycleState {
                resource_id,
                operation: ProcessLifecycleOperation::CommitResumed,
                state: current.state,
            });
        }
        current.state = ManagedProcessState::Running;
        Ok(())
    }

    pub fn begin_stopping_exact(&mut self, fence: &ManagedProcessFence) -> bool {
        let Some(current) = self.current.get_mut(&fence.resource.resource_id) else {
            return false;
        };
        if ManagedProcessFence::from_process(current) != *fence {
            return false;
        }
        match current.state {
            ManagedProcessState::Starting
            | ManagedProcessState::Running
            | ManagedProcessState::Failed => {
                current.state = ManagedProcessState::Stopping;
                true
            }
            ManagedProcessState::Stopping => true,
            ManagedProcessState::ZeroSettled | ManagedProcessState::Leaked => false,
        }
    }
}

#[allow(private_bounds)]
impl<J: JobMembership> ProcessRegistry<J> {
    /// Capture the current Job membership and mint a registry-owned snapshot.
    /// The caller supplies only the observation clock and freshness bound;
    /// revision, sequence, member identities, and validity come from this
    /// live registry/Job query.
    pub fn managed_resource_snapshot(
        &self,
        resource: ResourceFence,
        observed_at: Instant,
        max_age: Duration,
    ) -> Option<crate::process::ports::ManagedResourceSnapshot> {
        let resource_id = resource.resource_id;
        if self
            .current(resource_id)
            .is_none_or(|current| current.fence() != resource)
        {
            return None;
        }

        let current = self.current(resource_id)?;
        let fence = ManagedProcessFence::from_process(current);
        let state = current.state();
        let mut members = Vec::with_capacity(1 + current.known_members().len());
        let mut unknown_member_pids = Vec::new();
        let result = match current.job().active_process_ids() {
            Ok(mut active_pids) => {
                active_pids.sort_unstable();
                active_pids.dedup();
                let root_pid = current.root().id().pid();
                if !active_pids.contains(&root_pid) {
                    unknown_member_pids.push(root_pid);
                }
                for pid in active_pids {
                    match current.job().inspect_process(pid) {
                        Ok(member)
                            if member.identity().id().pid() == pid
                                && (pid != root_pid || member.identity() == current.root()) =>
                        {
                            members.push(member.identity().clone())
                        }
                        Ok(_) | Err(_) => unknown_member_pids.push(pid),
                    }
                }
                if unknown_member_pids.is_empty() {
                    CachedMembershipResult::Valid
                } else {
                    CachedMembershipResult::Failed(format!(
                        "{} managed Job member PIDs have unverified identity",
                        unknown_member_pids.len()
                    ))
                }
            }
            Err(error) => CachedMembershipResult::Failed(error),
        };
        members.sort_by(|left, right| {
            left.id()
                .pid()
                .cmp(&right.id().pid())
                .then_with(|| {
                    left.id()
                        .creation_time_100ns()
                        .cmp(&right.id().creation_time_100ns())
                })
                .then_with(|| {
                    left.canonical_executable()
                        .cmp(right.canonical_executable())
                })
        });
        members.dedup();

        let candidate = CachedMembershipObservation {
            fence: fence.clone(),
            state,
            members: members.clone(),
            unknown_member_pids: unknown_member_pids.clone(),
            result: result.clone(),
            membership_revision: 0,
            observation_sequence: 0,
        };
        let (membership_revision, observation_sequence) = {
            let mut cache = self.membership_cache.lock().ok()?;
            if let Some(previous) = cache.get(&resource_id).filter(|previous| {
                previous.fence == candidate.fence
                    && previous.state == candidate.state
                    && previous.members == candidate.members
                    && previous.unknown_member_pids == candidate.unknown_member_pids
                    && previous.result == candidate.result
            }) {
                (previous.membership_revision, previous.observation_sequence)
            } else {
                let membership_revision = self
                    .membership_revision
                    .fetch_add(1, Ordering::AcqRel)
                    .saturating_add(1);
                let observation_sequence = self
                    .observation_sequence
                    .fetch_add(1, Ordering::AcqRel)
                    .saturating_add(1);
                cache.insert(
                    resource_id,
                    CachedMembershipObservation {
                        membership_revision,
                        observation_sequence,
                        ..candidate
                    },
                );
                (membership_revision, observation_sequence)
            }
        };
        let membership = match result {
            CachedMembershipResult::Valid => {
                crate::process::ports::RegistryMembershipSnapshot::valid(
                    membership_revision,
                    observation_sequence,
                    observed_at,
                    max_age,
                )
            }
            CachedMembershipResult::Failed(detail) => {
                crate::process::ports::RegistryMembershipSnapshot::failed(
                    membership_revision,
                    observation_sequence,
                    detail,
                )
            }
        };
        Some(crate::process::ports::ManagedResourceSnapshot::new(
            fence, state, members, membership,
        ))
    }

    pub fn register(
        &mut self,
        mut process: RegisteredProcess<J>,
    ) -> Result<ManagedProcessFence, ProcessRegistrationFailure<J>> {
        let proposed = process.fence;
        if let Some(current) = self.current.get(&proposed.resource_id) {
            return Err(ProcessRegistrationFailure::new(
                ProcessRegistryError::ActiveGeneration {
                    current: current.fence,
                    proposed,
                },
                process,
            ));
        }

        let proposed_pid = process.root.id().pid();
        if let Some((existing_resource, _)) = self
            .current
            .iter()
            .find(|(_, current)| current.root.id().pid() == proposed_pid)
        {
            return Err(ProcessRegistrationFailure::new(
                ProcessRegistryError::DuplicateActivePid {
                    pid: proposed_pid,
                    existing_resource: *existing_resource,
                    proposed_resource: proposed.resource_id,
                },
                process,
            ));
        }

        let active_process_ids = match process.job.active_process_ids() {
            Ok(process_ids) => process_ids,
            Err(detail) => {
                return Err(ProcessRegistrationFailure::new(
                    ProcessRegistryError::MembershipQueryFailed {
                        resource_id: proposed.resource_id,
                        detail,
                    },
                    process,
                ));
            }
        };
        if !active_process_ids.contains(&proposed_pid) {
            return Err(ProcessRegistrationFailure::new(
                ProcessRegistryError::NotJobMember {
                    resource_id: proposed.resource_id,
                    pid: proposed_pid,
                },
                process,
            ));
        }

        let observed_root = match process.job.inspect_process(proposed_pid) {
            Ok(member) => member.identity().clone(),
            Err(detail) => {
                return Err(ProcessRegistrationFailure::new(
                    ProcessRegistryError::MembershipQueryFailed {
                        resource_id: proposed.resource_id,
                        detail,
                    },
                    process,
                ));
            }
        };
        if observed_root != *process.root() {
            return Err(ProcessRegistrationFailure::new(
                ProcessRegistryError::IdentityMismatch {
                    resource_id: proposed.resource_id,
                    expected: process.root().clone(),
                    observed: observed_root,
                },
                process,
            ));
        }

        let fence = ManagedProcessFence::from_process(&process);
        if let Err(detail) = process.job.bind_completion_fence(fence.clone()) {
            return Err(ProcessRegistrationFailure::new(
                ProcessRegistryError::CompletionNotificationsFailed {
                    resource_id: proposed.resource_id,
                    detail,
                },
                process,
            ));
        }

        if let Err(error) = self.runtime.install_current(proposed) {
            return Err(ProcessRegistrationFailure::new(
                ProcessRegistryError::Runtime(error),
                process,
            ));
        }
        self.current.insert(proposed.resource_id, process);
        Ok(fence)
    }

    pub(crate) fn drain_job_completions_until(
        &mut self,
        resource_id: ResourceId,
        absolute_deadline: Instant,
    ) -> Result<usize, ProcessRegistryError> {
        if Instant::now() >= absolute_deadline {
            return Err(ProcessRegistryError::CompletionNotificationsFailed {
                resource_id,
                detail: "Job completion drain exceeded teardown absolute deadline".to_string(),
            });
        }
        let messages = self
            .current
            .get(&resource_id)
            .map(|process| {
                process
                    .job
                    .drain_completion_messages_until(absolute_deadline)
            })
            .transpose()
            .map_err(
                |detail| ProcessRegistryError::CompletionNotificationsFailed {
                    resource_id,
                    detail,
                },
            )?
            .unwrap_or_default();
        if Instant::now() >= absolute_deadline {
            return Err(ProcessRegistryError::CompletionNotificationsFailed {
                resource_id,
                detail: "Job completion drain exceeded teardown absolute deadline".to_string(),
            });
        }
        let count = messages.len();
        for message in messages {
            if Instant::now() >= absolute_deadline {
                return Err(ProcessRegistryError::CompletionNotificationsFailed {
                    resource_id,
                    detail: "Job completion application exceeded teardown absolute deadline"
                        .to_string(),
                });
            }
            self.apply_job_completion(message);
        }
        if Instant::now() >= absolute_deadline {
            return Err(ProcessRegistryError::CompletionNotificationsFailed {
                resource_id,
                detail: "Job completion application exceeded teardown absolute deadline"
                    .to_string(),
            });
        }
        Ok(count)
    }

    /// Returns the pending receipt for a previously observed exact
    /// ACTIVE_PROCESS_ZERO completion.
    ///
    /// The receipt is retained by the registry until
    /// `settle_active_process_zero_exact` consumes it, so an adapter can carry
    /// it across its wait boundary without turning a caller boolean into proof.
    pub fn active_process_zero_proof_exact(
        &self,
        fence: &ManagedProcessFence,
    ) -> Result<ActiveProcessZeroProof, ProcessRegistryError> {
        let resource_id = fence.resource.resource_id;
        let Some(current) = self.current.get(&resource_id) else {
            return Err(ProcessRegistryError::StaleLifecycleFence {
                resource_id,
                operation: ProcessLifecycleOperation::ReleaseStopped,
            });
        };
        if ManagedProcessFence::from_process(current) != *fence {
            return Err(ProcessRegistryError::StaleLifecycleFence {
                resource_id,
                operation: ProcessLifecycleOperation::ReleaseStopped,
            });
        }
        current
            .pending_zero_proof
            .clone()
            .ok_or(ProcessRegistryError::ActiveProcessZeroUnproved { resource_id })
    }

    /// Settle a registry-issued ACTIVE_PROCESS_ZERO receipt only after
    /// rechecking the exact fence and querying the Job for authoritative empty
    /// membership.
    ///
    /// Ok(false) means the completion was not enough because the Job still has
    /// members. The caller must keep the Job/completion handles alive and must
    /// not call release_stopped_exact in that case.
    pub fn settle_active_process_zero_exact(
        &mut self,
        proof: ActiveProcessZeroProof,
    ) -> Result<bool, ProcessRegistryError> {
        let absolute_deadline = Instant::now()
            .checked_add(Duration::from_secs(1))
            .ok_or_else(|| ProcessRegistryError::MembershipQueryFailed {
                resource_id: proof.fence.resource.resource_id,
                detail: "ACTIVE_PROCESS_ZERO settlement deadline overflow".to_string(),
            })?;
        self.settle_active_process_zero_exact_until(proof, absolute_deadline)
    }

    pub(crate) fn settle_active_process_zero_exact_until(
        &mut self,
        proof: ActiveProcessZeroProof,
        absolute_deadline: Instant,
    ) -> Result<bool, ProcessRegistryError> {
        let resource_id = proof.fence.resource.resource_id;
        if Instant::now() >= absolute_deadline {
            return Err(ProcessRegistryError::MembershipQueryFailed {
                resource_id,
                detail: "ACTIVE_PROCESS_ZERO settlement exceeded teardown absolute deadline"
                    .to_string(),
            });
        }
        let Some(current) = self.current.get_mut(&resource_id) else {
            return Err(ProcessRegistryError::StaleLifecycleFence {
                resource_id,
                operation: ProcessLifecycleOperation::ReleaseStopped,
            });
        };
        if ManagedProcessFence::from_process(current) != proof.fence {
            return Err(ProcessRegistryError::StaleLifecycleFence {
                resource_id,
                operation: ProcessLifecycleOperation::ReleaseStopped,
            });
        }
        if current.pending_zero_proof.as_ref() != Some(&proof) {
            return Err(ProcessRegistryError::ActiveProcessZeroUnproved { resource_id });
        }

        let mut active_process_ids = current
            .job
            .active_process_ids_until(absolute_deadline)
            .map_err(|detail| ProcessRegistryError::MembershipQueryFailed {
                resource_id,
                detail,
            })?;
        active_process_ids.sort_unstable();
        active_process_ids.dedup();
        if Instant::now() >= absolute_deadline {
            return Err(ProcessRegistryError::MembershipQueryFailed {
                resource_id,
                detail: "ACTIVE_PROCESS_ZERO settlement exceeded teardown absolute deadline"
                    .to_string(),
            });
        }
        if !active_process_ids.is_empty() {
            current.pending_zero_proof = None;
            current.authoritative_zero_settled = false;
            current.settled_zero_nonce = None;
            current.state = current
                .pending_zero_prior_state
                .take()
                .unwrap_or(ManagedProcessState::Leaked);
            return Ok(false);
        }

        current.pending_zero_proof = None;
        current.pending_zero_prior_state = None;
        current.known_members.clear();
        current.unknown_member_pids.clear();
        current.state = ManagedProcessState::ZeroSettled;
        current.authoritative_zero_settled = true;
        current.settled_zero_nonce = Some(proof.nonce);
        Ok(true)
    }

    /// Performs the final receiver-proof plus authoritative empty-membership
    /// settlement while binding the result to the exact teardown action epoch.
    /// The returned authority is the only input accepted by final release.
    pub fn mint_teardown_release_authority_exact(
        &mut self,
        ticket: &TeardownTicket,
        proof: ActiveProcessZeroProof,
    ) -> Result<TeardownReleaseAuthority, ProcessRegistryError> {
        let absolute_deadline = Instant::now()
            .checked_add(Duration::from_secs(1))
            .ok_or_else(|| ProcessRegistryError::MembershipQueryFailed {
                resource_id: ticket.resource_id(),
                detail: "teardown release authority deadline overflow".to_string(),
            })?;
        self.mint_teardown_release_authority_exact_until(ticket, proof, absolute_deadline)
    }

    pub(crate) fn mint_teardown_release_authority_exact_until(
        &mut self,
        ticket: &TeardownTicket,
        proof: ActiveProcessZeroProof,
        absolute_deadline: Instant,
    ) -> Result<TeardownReleaseAuthority, ProcessRegistryError> {
        if Instant::now() >= absolute_deadline {
            return Err(ProcessRegistryError::MembershipQueryFailed {
                resource_id: ticket.resource_id(),
                detail: "teardown release authority exceeded teardown absolute deadline"
                    .to_string(),
            });
        }
        if proof.fence() != ticket.fence() {
            return Err(ProcessRegistryError::TeardownReleaseAuthorityMismatch {
                resource_id: ticket.resource_id(),
            });
        }
        if !self.settle_active_process_zero_exact_until(proof.clone(), absolute_deadline)? {
            return Err(ProcessRegistryError::InvalidLifecycleState {
                resource_id: ticket.resource_id(),
                operation: ProcessLifecycleOperation::ReleaseStopped,
                state: self
                    .current(ticket.resource_id())
                    .map(|current| current.state)
                    .unwrap_or(ManagedProcessState::Leaked),
            });
        }
        Ok(TeardownReleaseAuthority::from_registry(ticket, proof.nonce))
    }

    /// Applies a caller-supplied observation for diagnostics only. It validates
    /// the current generation but never mutates lifecycle state or creates a
    /// zero proof.
    pub fn apply_job_observation(&mut self, observation: JobCompletionObservation) -> bool {
        let resource_id = observation.fence.resource.resource_id;
        self.current
            .get(&resource_id)
            .map(|current| ManagedProcessFence::from_process(current) == observation.fence)
            .unwrap_or(false)
    }

    /// Applies one message emitted by the concrete managed Job receiver.
    /// `JobCompletionMessage` has no public constructor; this boundary is
    /// therefore unforgeable by caller-owned membership adapters.
    pub fn apply_job_completion(&mut self, message: JobCompletionMessage) -> bool {
        let resource_id = message.fence.resource.resource_id;
        let Some(current) = self.current.get_mut(&resource_id) else {
            return false;
        };
        if ManagedProcessFence::from_process(current) != message.fence {
            return false;
        }

        match message.event {
            JobCompletionEvent::NewProcess { .. } => {
                if current.state == ManagedProcessState::Starting {
                    current.state = ManagedProcessState::Running;
                }
            }
            JobCompletionEvent::ExitProcess { .. } => {
                // Completion-packet PIDs are scheduling hints, never stable
                // identity. The next authoritative Job query updates members.
            }
            JobCompletionEvent::AbnormalExitProcess { .. } => {
                if current.state != ManagedProcessState::ZeroSettled {
                    current.state = ManagedProcessState::Failed;
                }
            }
            JobCompletionEvent::ActiveProcessZero => {
                if current.state != ManagedProcessState::ZeroSettled {
                    if current.pending_zero_prior_state.is_none() {
                        current.pending_zero_prior_state = Some(current.state);
                    }
                    current.authoritative_zero_settled = false;
                    current.settled_zero_nonce = None;
                    let fence = ManagedProcessFence::from_process(current);
                    let nonce = current.next_zero_proof_nonce;
                    current.next_zero_proof_nonce =
                        current.next_zero_proof_nonce.wrapping_add(1).max(1);
                    current.pending_zero_proof =
                        Some(ActiveProcessZeroProof::from_completion(fence, nonce));
                    // The completion packet is receiver-authenticated but is
                    // still only a scheduling hint.  Membership is queried at
                    // the explicit, deadline-bound reconciliation/settlement
                    // boundary; never perform an unbounded query here.
                }
            }
            JobCompletionEvent::Limit { message_id, pid } => {
                current.last_limit = Some((message_id, pid));
                if current.state != ManagedProcessState::ZeroSettled {
                    current.state = ManagedProcessState::Failed;
                }
            }
            JobCompletionEvent::MonitorFailed { .. } => {
                if current.state != ManagedProcessState::ZeroSettled {
                    current.state = ManagedProcessState::Leaked;
                }
            }
        }
        true
    }

    pub fn reconcile_membership(
        &mut self,
        resource_id: ResourceId,
    ) -> Result<(), ProcessRegistryError> {
        let absolute_deadline = Instant::now()
            .checked_add(Duration::from_secs(1))
            .ok_or_else(|| ProcessRegistryError::MembershipQueryFailed {
                resource_id,
                detail: "Job membership reconciliation deadline overflow".to_string(),
            })?;
        self.reconcile_membership_until(resource_id, absolute_deadline)
    }

    pub(crate) fn reconcile_membership_until(
        &mut self,
        resource_id: ResourceId,
        absolute_deadline: Instant,
    ) -> Result<(), ProcessRegistryError> {
        if Instant::now() >= absolute_deadline {
            return Err(ProcessRegistryError::MembershipQueryFailed {
                resource_id,
                detail: "Job membership reconciliation exceeded teardown absolute deadline"
                    .to_string(),
            });
        }
        let Some(current) = self.current.get_mut(&resource_id) else {
            return Ok(());
        };
        let mut active_pids = current
            .job
            .active_process_ids_until(absolute_deadline)
            .map_err(|detail| ProcessRegistryError::MembershipQueryFailed {
                resource_id,
                detail,
            })?;
        active_pids.sort_unstable();
        active_pids.dedup();
        if current.pending_zero_proof.is_some() {
            if active_pids.is_empty() {
                current.pending_zero_prior_state = None;
                current.known_members.clear();
                current.unknown_member_pids.clear();
                // Empty membership is not final release.  Keep the live Job
                // entry visibly nonterminal until exact authority consumes it.
                current.state = ManagedProcessState::ZeroSettled;
                if Instant::now() >= absolute_deadline {
                    return Err(ProcessRegistryError::MembershipQueryFailed {
                        resource_id,
                        detail: "Job membership reconciliation exceeded teardown absolute deadline"
                            .to_string(),
                    });
                }
                return Ok(());
            }
            current.pending_zero_proof = None;
            current.state = current
                .pending_zero_prior_state
                .take()
                .unwrap_or(ManagedProcessState::Leaked);
        }
        if !active_pids.is_empty() && current.state == ManagedProcessState::Starting {
            current.state = ManagedProcessState::Running;
        }

        let mut known_members = Vec::with_capacity(active_pids.len());
        let mut unknown_member_pids = Vec::new();
        for pid in active_pids {
            if Instant::now() >= absolute_deadline {
                return Err(ProcessRegistryError::MembershipQueryFailed {
                    resource_id,
                    detail: "Job membership reconciliation exceeded teardown absolute deadline"
                        .to_string(),
                });
            }
            let observation = current.job.inspect_process_until(pid, absolute_deadline);
            if Instant::now() >= absolute_deadline {
                return Err(ProcessRegistryError::MembershipQueryFailed {
                    resource_id,
                    detail: "Job membership reconciliation exceeded teardown absolute deadline"
                        .to_string(),
                });
            }
            match observation {
                Ok(member) if member.identity.id().pid() == pid => known_members.push(member),
                Ok(_) | Err(_) => unknown_member_pids.push(pid),
            }
        }
        known_members.sort_by_key(|member| {
            (
                member.identity.id().pid(),
                member.identity.id().creation_time_100ns(),
            )
        });
        if Instant::now() >= absolute_deadline {
            return Err(ProcessRegistryError::MembershipQueryFailed {
                resource_id,
                detail: "Job membership reconciliation exceeded teardown absolute deadline"
                    .to_string(),
            });
        }
        current.known_members = known_members;
        current.unknown_member_pids = unknown_member_pids;
        if Instant::now() >= absolute_deadline {
            return Err(ProcessRegistryError::MembershipQueryFailed {
                resource_id,
                detail: "Job membership reconciliation exceeded teardown absolute deadline"
                    .to_string(),
            });
        }
        Ok(())
    }

    pub fn classify_root(&self, observed: &ManagedProcessIdentity) -> ProcessClassification {
        for current in self.current.values() {
            if current.root.matches_root(observed) {
                let expected = ManagedProcessFence::from_process(current);
                return match current.job.active_process_ids() {
                    Ok(process_ids) if process_ids.contains(&observed.id().pid()) => {
                        match current.job.inspect_process(observed.id().pid()) {
                            Ok(member)
                                if member.identity() == observed
                                    && member.identity() == current.root() =>
                            {
                                ProcessClassification::Managed(expected)
                            }
                            Ok(_) => ProcessClassification::ReconciliationFault {
                                expected,
                                observed: observed.clone(),
                                reason: OwnershipFault::IdentityMismatch,
                            },
                            Err(detail) => ProcessClassification::ReconciliationFault {
                                expected,
                                observed: observed.clone(),
                                reason: OwnershipFault::MembershipQueryFailed { detail },
                            },
                        }
                    }
                    Ok(_) => ProcessClassification::ReconciliationFault {
                        expected,
                        observed: observed.clone(),
                        reason: OwnershipFault::NotJobMember,
                    },
                    Err(detail) => ProcessClassification::ReconciliationFault {
                        expected,
                        observed: observed.clone(),
                        reason: OwnershipFault::MembershipQueryFailed { detail },
                    },
                };
            }
            if current.root.id().pid() == observed.id().pid() {
                return ProcessClassification::ReconciliationFault {
                    expected: ManagedProcessFence::from_process(current),
                    observed: observed.clone(),
                    reason: OwnershipFault::IdentityMismatch,
                };
            }
        }
        ProcessClassification::External
    }
}

#[cfg(test)]
mod release_authority_tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use crate::domain::id::{OperationId, ResourceId};
    use crate::domain::operation::ResourceFence;
    use crate::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
    use crate::process::teardown::{TeardownScope, TeardownTicket};

    use super::{
        JobCompletionEvent, JobCompletionMessage, JobMemberInfo, JobMembership,
        ManagedProcessFence, ProcessDisplayLabel, ProcessRegistry, ProcessRegistryError,
        RegisteredProcess, UnregisterOutcome,
    };

    #[derive(Debug)]
    struct RetryReleaseState {
        root: ManagedProcessIdentity,
        active: Vec<u32>,
        shutdown_attempts: usize,
    }

    #[derive(Debug, Clone)]
    struct RetryReleaseJob(Arc<Mutex<RetryReleaseState>>);

    impl JobMembership for RetryReleaseJob {
        fn active_process_ids(&self) -> Result<Vec<u32>, String> {
            Ok(self.0.lock().expect("retry Job state").active.clone())
        }

        fn inspect_process(&self, pid: u32) -> Result<JobMemberInfo, String> {
            let state = self.0.lock().expect("retry Job state");
            if pid != state.root.id().pid() {
                return Err(format!("PID {pid} is not the registered root"));
            }
            Ok(JobMemberInfo::new(state.root.clone(), None))
        }

        fn bind_completion_fence(&mut self, _fence: ManagedProcessFence) -> Result<(), String> {
            Ok(())
        }

        fn shutdown_for_release(&mut self) -> Result<(), String> {
            let mut state = self.0.lock().expect("retry Job state");
            state.shutdown_attempts += 1;
            if state.shutdown_attempts == 1 {
                Err("listener has not acknowledged cancellation yet".to_string())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn failed_listener_release_keeps_same_authority_retryable() {
        let executable = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("test.exe"));
        let root = ManagedProcessIdentity::new(
            ManagedProcessId::new(42_424, 7).expect("managed process id"),
            executable,
        )
        .expect("managed process identity");
        let state = Arc::new(Mutex::new(RetryReleaseState {
            root: root.clone(),
            active: vec![root.id().pid()],
            shutdown_attempts: 0,
        }));
        let resource_id = ResourceId::new();
        let mut registry = ProcessRegistry::new();
        let fence = registry
            .register(RegisteredProcess::new(
                ResourceFence::new(resource_id, 1),
                ProcessOwner::Host,
                root,
                ProcessDisplayLabel::new("retryable listener release").expect("display label"),
                RetryReleaseJob(Arc::clone(&state)),
            ))
            .expect("register retry Job");

        state.lock().expect("retry Job state").active.clear();
        assert!(registry.apply_job_completion(JobCompletionMessage {
            fence: fence.clone(),
            event: JobCompletionEvent::ActiveProcessZero,
        }));
        let ticket = TeardownTicket::new(OperationId::new(), TeardownScope::Host, 9, fence.clone())
            .expect("host teardown ticket");
        let proof = registry
            .active_process_zero_proof_exact(&fence)
            .expect("receiver-owned zero proof");
        let authority = registry
            .mint_teardown_release_authority_exact(&ticket, proof)
            .expect("release authority");

        assert!(matches!(
            registry.release_stopped_with_authority_until(&ticket, &authority, Instant::now()),
            Err(ProcessRegistryError::JobShutdownFailed { .. })
        ));
        assert!(registry.current(resource_id).is_some());
        assert_eq!(state.lock().expect("retry Job state").shutdown_attempts, 0);

        assert!(matches!(
            registry.release_stopped_with_authority(&ticket, &authority),
            Err(ProcessRegistryError::JobShutdownFailed { .. })
        ));
        assert!(registry.current(resource_id).is_some());
        assert!(matches!(
            registry.release_stopped_with_authority(&ticket, &authority),
            Ok(UnregisterOutcome::Removed(_))
        ));
        assert!(registry.current(resource_id).is_none());
        assert_eq!(state.lock().expect("retry Job state").shutdown_attempts, 2);
    }
}
