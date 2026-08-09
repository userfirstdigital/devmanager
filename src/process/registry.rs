//! Generation-fenced ownership of managed process roots.

use std::collections::BTreeMap;
use std::fmt;

use crate::domain::id::ResourceId;
use crate::domain::operation::ResourceFence;
use crate::kernel::{RuntimeRegistry, RuntimeRegistryError};
use crate::process::identity::{ManagedProcessIdentity, ProcessOwner};

pub const MAX_PROCESS_DISPLAY_LABEL_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessDisplayLabel(String);

impl ProcessDisplayLabel {
    pub fn new(value: impl Into<String>) -> Result<Self, ProcessDisplayLabelError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ProcessDisplayLabelError::Empty);
        }
        if value.len() > MAX_PROCESS_DISPLAY_LABEL_BYTES {
            return Err(ProcessDisplayLabelError::TooLong {
                actual: value.len(),
                max: MAX_PROCESS_DISPLAY_LABEL_BYTES,
            });
        }
        Ok(Self(value))
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
pub trait JobMembership {
    fn active_process_ids(&self) -> Result<Vec<u32>, String>;

    /// Terminate the owned Job tree, never a PID-selected process.
    fn terminate_tree(&self) -> Result<(), String> {
        Err("Job tree termination is unavailable for this membership".to_string())
    }

    fn inspect_process(&self, pid: u32) -> Result<JobMemberInfo, String> {
        Err(format!("process identity for PID {pid} is inaccessible"))
    }

    fn bind_completion_fence(&mut self, _fence: ManagedProcessFence) -> Result<(), String> {
        Ok(())
    }

    fn drain_completion_messages(&self) -> Vec<JobCompletionMessage> {
        Vec::new()
    }
}

impl JobMembership for crate::process::job::ManagedProcessJob {
    fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        crate::process::job::ManagedProcessJob::active_process_ids(self)
    }

    fn terminate_tree(&self) -> Result<(), String> {
        crate::process::job::ManagedProcessJob::terminate_tree(self)
    }

    fn inspect_process(&self, pid: u32) -> Result<JobMemberInfo, String> {
        crate::process::job::ManagedProcessJob::inspect_process(self, pid)
    }

    fn bind_completion_fence(&mut self, fence: ManagedProcessFence) -> Result<(), String> {
        crate::process::job::ManagedProcessJob::bind_completion_fence(self, fence)
    }

    fn drain_completion_messages(&self) -> Vec<JobCompletionMessage> {
        crate::process::job::ManagedProcessJob::drain_completion_messages(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedProcessState {
    Starting,
    Running,
    Stopping,
    Stopped,
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

    /// Explicit seam for pure coordinator tests. Production adapters must
    /// obtain proofs from `ProcessRegistry::active_process_zero_proof_exact`.
    #[doc(hidden)]
    pub fn for_test(fence: ManagedProcessFence) -> Self {
        Self { fence, nonce: 0 }
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
    next_zero_proof_nonce: u64,
}

impl<J> RegisteredProcess<J> {
    pub fn new(
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

    pub fn job(&self) -> &J {
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
    pub fn new(resource: ResourceFence, owner: ProcessOwner, root: ManagedProcessIdentity) -> Self {
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
    ActiveProcessZeroUnproved {
        resource_id: ResourceId,
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
            Self::ActiveProcessZeroUnproved { resource_id } => write!(
                f,
                "resource {resource_id} has no matching ACTIVE_PROCESS_ZERO completion proof"
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
            | Self::ActiveProcessZeroUnproved { .. }
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

#[derive(Debug)]
pub struct ProcessRegistry<J> {
    runtime: RuntimeRegistry,
    current: BTreeMap<ResourceId, RegisteredProcess<J>>,
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
        self.take_exact_in_state(
            fence,
            ManagedProcessState::Stopped,
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
            ManagedProcessState::Stopped | ManagedProcessState::Leaked => false,
        }
    }
}

impl<J: JobMembership> ProcessRegistry<J> {
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

    pub fn drain_job_completions(&mut self, resource_id: ResourceId) -> usize {
        let messages = self
            .current
            .get(&resource_id)
            .map(|process| process.job.drain_completion_messages())
            .unwrap_or_default();
        let count = messages.len();
        for message in messages {
            self.apply_job_completion(message);
        }
        count
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
        let resource_id = proof.fence.resource.resource_id;
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

        let mut active_process_ids = current.job.active_process_ids().map_err(|detail| {
            ProcessRegistryError::MembershipQueryFailed {
                resource_id,
                detail,
            }
        })?;
        active_process_ids.sort_unstable();
        active_process_ids.dedup();
        if !active_process_ids.is_empty() {
            current.pending_zero_proof = None;
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
        current.state = ManagedProcessState::Stopped;
        Ok(true)
    }

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
                if current.state != ManagedProcessState::Stopped {
                    current.state = ManagedProcessState::Failed;
                }
            }
            JobCompletionEvent::ActiveProcessZero => {
                if current.state != ManagedProcessState::Stopped {
                    let fence = ManagedProcessFence::from_process(current);
                    let nonce = current.next_zero_proof_nonce;
                    current.next_zero_proof_nonce =
                        current.next_zero_proof_nonce.wrapping_add(1).max(1);
                    current.pending_zero_proof =
                        Some(ActiveProcessZeroProof::from_completion(fence, nonce));
                    match current.job.active_process_ids() {
                        Ok(process_ids) if process_ids.is_empty() => {
                            current.pending_zero_prior_state = None;
                            current.known_members.clear();
                            current.unknown_member_pids.clear();
                            current.state = ManagedProcessState::Stopped;
                        }
                        Ok(_) => {
                            current.pending_zero_proof = None;
                            if let Some(prior_state) = current.pending_zero_prior_state.take() {
                                current.state = prior_state;
                            }
                        }
                        Err(_) => {
                            if current.pending_zero_prior_state.is_none() {
                                current.pending_zero_prior_state = Some(current.state);
                            }
                            current.state = ManagedProcessState::Leaked;
                        }
                    }
                }
            }
            JobCompletionEvent::Limit { message_id, pid } => {
                current.last_limit = Some((message_id, pid));
                if current.state != ManagedProcessState::Stopped {
                    current.state = ManagedProcessState::Failed;
                }
            }
            JobCompletionEvent::MonitorFailed { .. } => {
                if current.state != ManagedProcessState::Stopped {
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
        let Some(current) = self.current.get_mut(&resource_id) else {
            return Ok(());
        };
        let mut active_pids = current.job.active_process_ids().map_err(|detail| {
            ProcessRegistryError::MembershipQueryFailed {
                resource_id,
                detail,
            }
        })?;
        active_pids.sort_unstable();
        active_pids.dedup();
        if current.pending_zero_proof.is_some() {
            if active_pids.is_empty() {
                current.pending_zero_prior_state = None;
                current.known_members.clear();
                current.unknown_member_pids.clear();
                current.state = ManagedProcessState::Stopped;
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
            match current.job.inspect_process(pid) {
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
        current.known_members = known_members;
        current.unknown_member_pids = unknown_member_pids;
        Ok(())
    }

    pub fn classify_root(&self, observed: &ManagedProcessIdentity) -> ProcessClassification {
        for current in self.current.values() {
            if current.root.matches_root(observed) {
                let expected = ManagedProcessFence::from_process(current);
                return match current.job.active_process_ids() {
                    Ok(process_ids) if process_ids.contains(&observed.id().pid()) => {
                        ProcessClassification::Managed(expected)
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
