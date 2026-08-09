//! Immutable port observations and generation-fenced managed ownership.
//!
//! Operating-system probing belongs in [`crate::services::ports_service`].
//! This module only joins an already-captured observation to a process
//! registry snapshot, so callers can project the result into any domain/UI
//! snapshot without doing work on a render or input path.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::domain::id::ResourceId;
use crate::domain::operation::ResourceFence;
use crate::process::identity::ManagedProcessId;
use crate::process::registry::{ManagedProcessState, ProcessRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ListenerIdentity {
    pid: u32,
    creation_time_100ns: u64,
}

impl ListenerIdentity {
    pub fn new(pid: u32, creation_time_100ns: u64) -> Result<Self, ListenerIdentityError> {
        if pid == 0 {
            return Err(ListenerIdentityError::ZeroPid);
        }
        if creation_time_100ns == 0 {
            return Err(ListenerIdentityError::ZeroCreationTime);
        }
        Ok(Self {
            pid,
            creation_time_100ns,
        })
    }

    pub fn pid(self) -> u32 {
        self.pid
    }

    pub fn creation_time_100ns(self) -> u64 {
        self.creation_time_100ns
    }

    fn managed_id(self) -> ManagedProcessId {
        ManagedProcessId::new(self.pid, self.creation_time_100ns)
            .expect("ListenerIdentity validates its managed process identity")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerIdentityError {
    ZeroPid,
    ZeroCreationTime,
}

impl fmt::Display for ListenerIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroPid => write!(formatter, "listener PID must be non-zero"),
            Self::ZeroCreationTime => {
                write!(formatter, "listener creation time must be non-zero")
            }
        }
    }
}

impl std::error::Error for ListenerIdentityError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortObservation {
    Free,
    Listener(ListenerIdentity),
    ProbeError(String),
}

impl PortObservation {
    pub fn listener(&self) -> Option<ListenerIdentity> {
        match self {
            Self::Listener(listener) => Some(*listener),
            Self::Free | Self::ProbeError(_) => None,
        }
    }
}

/// An immutable, batched result of one listener-table probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInventorySnapshot {
    observations: Arc<BTreeMap<u16, PortObservation>>,
}

impl PortInventorySnapshot {
    pub fn new(observations: BTreeMap<u16, PortObservation>) -> Self {
        Self {
            observations: Arc::new(observations),
        }
    }

    pub fn probe_failure(ports: impl IntoIterator<Item = u16>, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        let observations = ports
            .into_iter()
            .map(|port| (port, PortObservation::ProbeError(detail.clone())))
            .collect();
        Self::new(observations)
    }

    pub fn observation(&self, port: u16) -> Option<&PortObservation> {
        self.observations.get(&port)
    }

    pub fn observations(&self) -> &BTreeMap<u16, PortObservation> {
        &self.observations
    }

    pub fn iter(&self) -> impl Iterator<Item = (&u16, &PortObservation)> {
        self.observations.iter()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedPortHealth {
    Ready,
    NotReady,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortTarget {
    pub port: u16,
    pub resource: ResourceFence,
    pub health: ManagedPortHealth,
}

impl PortTarget {
    pub fn new(port: u16, resource: ResourceFence, health: ManagedPortHealth) -> Self {
        Self {
            port,
            resource,
            health,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortStatusKind {
    /// The exact managed generation is still in its launch phase.
    Starting,
    /// The listener identity is a member of the exact managed generation and
    /// the caller has supplied application-level readiness evidence.
    ManagedHealthy,
    /// A listener exists, but it is not owned by the requested resource
    /// generation.
    External,
    /// No listener was observed and no managed launch is in progress.
    Stopped,
    /// The listener state could not be established safely.
    ProbeError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortStatus {
    pub port: u16,
    pub resource: ResourceFence,
    pub kind: PortStatusKind,
    pub listener: Option<ListenerIdentity>,
    pub error: Option<String>,
}

impl PortStatus {
    pub fn kind(&self) -> PortStatusKind {
        self.kind
    }

    pub fn listener(&self) -> Option<ListenerIdentity> {
        self.listener
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedResourceSnapshot {
    resource: ResourceFence,
    state: ManagedProcessState,
    members: Arc<[ManagedProcessId]>,
}

impl ManagedResourceSnapshot {
    pub fn resource(&self) -> ResourceFence {
        self.resource
    }

    pub fn resource_id(&self) -> ResourceId {
        self.resource.resource_id
    }

    pub fn generation(&self) -> u64 {
        self.resource.runtime_generation
    }

    pub fn state(&self) -> ManagedProcessState {
        self.state
    }

    pub fn member_identities(&self) -> &[ManagedProcessId] {
        &self.members
    }

    fn owns(&self, listener: ListenerIdentity) -> bool {
        self.members.contains(&listener.managed_id())
    }
}

/// Copy only the identity-bearing, read-only part of one exact registry entry.
///
/// The current entry must match both the requested [`ResourceId`] and runtime
/// generation. Its root and every known Job member are retained as full
/// PID-plus-creation identities; a PID match by itself is never ownership.
pub fn registered_resource_snapshot<J>(
    registry: &ProcessRegistry<J>,
    resource: ResourceFence,
) -> Option<ManagedResourceSnapshot> {
    let current = registry.current(resource.resource_id)?;
    if current.fence() != resource {
        return None;
    }

    let mut members = Vec::with_capacity(1 + current.known_members().len());
    members.push(current.root().id());
    members.extend(
        current
            .known_members()
            .iter()
            .map(|member| member.identity().id()),
    );
    members.sort_unstable_by_key(|identity| (identity.pid(), identity.creation_time_100ns()));
    members.dedup();

    Some(ManagedResourceSnapshot {
        resource,
        state: current.state(),
        members: Arc::from(members.into_boxed_slice()),
    })
}

pub fn project_port_status(
    target: &PortTarget,
    observation: &PortObservation,
    managed: Option<&ManagedResourceSnapshot>,
) -> PortStatus {
    if managed.is_some_and(|resource| {
        resource.resource() == target.resource && resource.state() == ManagedProcessState::Starting
    }) {
        return PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::Starting,
            listener: observation.listener(),
            error: None,
        };
    }

    match observation {
        PortObservation::Free => PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::Stopped,
            listener: None,
            error: None,
        },
        PortObservation::ProbeError(detail) => PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::ProbeError,
            listener: None,
            error: Some(detail.clone()),
        },
        PortObservation::Listener(listener) => {
            let exact_owner = managed.filter(|resource| resource.resource() == target.resource);
            if exact_owner.is_some_and(|resource| {
                resource.state() == ManagedProcessState::Running
                    && target.health == ManagedPortHealth::Ready
                    && resource.owns(*listener)
            }) {
                PortStatus {
                    port: target.port,
                    resource: target.resource,
                    kind: PortStatusKind::ManagedHealthy,
                    listener: Some(*listener),
                    error: None,
                }
            } else if exact_owner.is_some_and(|resource| resource.owns(*listener)) {
                PortStatus {
                    port: target.port,
                    resource: target.resource,
                    kind: PortStatusKind::ProbeError,
                    listener: Some(*listener),
                    error: Some("managed listener is not health-ready".to_string()),
                }
            } else {
                PortStatus {
                    port: target.port,
                    resource: target.resource,
                    kind: PortStatusKind::External,
                    listener: Some(*listener),
                    error: None,
                }
            }
        }
    }
}

pub fn project_port_status_from_snapshot(
    target: &PortTarget,
    snapshot: &PortInventorySnapshot,
    managed: Option<&ManagedResourceSnapshot>,
) -> PortStatus {
    let Some(observation) = snapshot.observation(target.port) else {
        return PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::ProbeError,
            listener: None,
            error: Some("port was not included in the cached inventory snapshot".to_string()),
        };
    };
    project_port_status(target, observation, managed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortStartError {
    NotScanned {
        port: u16,
    },
    ProbeFailed {
        port: u16,
        detail: String,
    },
    OccupiedExternal {
        port: u16,
        listener: ListenerIdentity,
    },
}

impl fmt::Display for PortStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotScanned { port } => {
                write!(formatter, "port {port} has no cached listener observation")
            }
            Self::ProbeFailed { port, detail } => {
                write!(
                    formatter,
                    "could not establish whether port {port} is free: {detail}"
                )
            }
            Self::OccupiedExternal { port, listener } => write!(
                formatter,
                "port {port} is occupied by external listener PID {} (creation {})",
                listener.pid(),
                listener.creation_time_100ns()
            ),
        }
    }
}

impl std::error::Error for PortStartError {}

/// Admit a managed launch only when the immutable scan proved the port free.
///
/// This function has no process-control capability. In particular, an
/// external listener is reported with evidence and is never assigned,
/// adopted, signaled, or killed as part of the rejection.
pub fn ensure_managed_start_allowed(
    snapshot: &PortInventorySnapshot,
    port: u16,
) -> Result<(), PortStartError> {
    match snapshot.observation(port) {
        None => Err(PortStartError::NotScanned { port }),
        Some(PortObservation::Free) => Ok(()),
        Some(PortObservation::ProbeError(detail)) => Err(PortStartError::ProbeFailed {
            port,
            detail: detail.clone(),
        }),
        Some(PortObservation::Listener(listener)) => Err(PortStartError::OccupiedExternal {
            port,
            listener: *listener,
        }),
    }
}
