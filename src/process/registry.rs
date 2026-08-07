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
}

impl JobMembership for crate::process::job::ManagedProcessJob {
    fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        crate::process::job::ManagedProcessJob::active_process_ids(self)
    }
}

#[derive(Debug)]
pub struct RegisteredProcess<J> {
    fence: ResourceFence,
    owner: ProcessOwner,
    root: ManagedProcessIdentity,
    display_label: ProcessDisplayLabel,
    job: J,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedProcessFence {
    resource: ResourceFence,
    owner: ProcessOwner,
    root: ManagedProcessIdentity,
}

impl ManagedProcessFence {
    fn from_process<J>(process: &RegisteredProcess<J>) -> Self {
        Self {
            resource: process.fence,
            owner: process.owner,
            root: process.root.clone(),
        }
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
            | Self::MembershipQueryFailed { .. } => None,
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

    pub fn unregister_exact(
        &mut self,
        fence: &ManagedProcessFence,
    ) -> Result<UnregisterOutcome<J>, ProcessRegistryError> {
        let resource_id = fence.resource.resource_id;
        let Some(current) = self.current.get(&resource_id) else {
            return Ok(UnregisterOutcome::Stale);
        };
        if ManagedProcessFence::from_process(current) != *fence {
            return Ok(UnregisterOutcome::Stale);
        }

        self.runtime.retire(fence.resource)?;
        let removed = self
            .current
            .remove(&resource_id)
            .expect("exact current registry entry was checked before removal");
        Ok(UnregisterOutcome::Removed(removed))
    }
}

impl<J: JobMembership> ProcessRegistry<J> {
    pub fn register(
        &mut self,
        process: RegisteredProcess<J>,
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

        if let Err(error) = self.runtime.install_current(proposed) {
            return Err(ProcessRegistrationFailure::new(
                ProcessRegistryError::Runtime(error),
                process,
            ));
        }
        let fence = ManagedProcessFence::from_process(&process);
        self.current.insert(proposed.resource_id, process);
        Ok(fence)
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
