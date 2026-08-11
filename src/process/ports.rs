//! Immutable port observations and generation-fenced managed ownership.
//!
//! Operating-system probing belongs in [`crate::services::ports_service`].
//! This module only joins an already-captured observation to a process
//! registry snapshot, so callers can project the result into any domain/UI
//! snapshot without doing work on a render or input path.

use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::domain::id::ResourceId;
use crate::domain::operation::ResourceFence;
use crate::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
use crate::process::registry::{ManagedProcessFence, ManagedProcessState, ProcessRegistry};

pub const MAX_PORTS_PER_SCAN: usize = 256;
pub const MAX_ENDPOINTS_PER_SCAN: usize = 4096;
pub const MAX_SCAN_ERRORS: usize = 64;
pub const MAX_SCAN_WAITERS: usize = 64;
pub const DEFAULT_FREE_PROOF_MAX_AGE: Duration = Duration::from_secs(5);
pub const DEFAULT_MEMBERSHIP_MAX_AGE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct ScanCancellation {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl ScanCancellation {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline,
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || Instant::now() >= self.deadline
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortScanError {
    TimedOut,
    Cancelled,
    Shutdown,
    TooManyPorts { actual: usize, max: usize },
    QueueFull { actual: usize, max: usize },
    Scan(String),
}

impl fmt::Display for PortScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => write!(formatter, "port inventory scan timed out"),
            Self::Cancelled => write!(formatter, "port inventory scan was cancelled"),
            Self::Shutdown => write!(formatter, "port inventory is shut down"),
            Self::TooManyPorts { actual, max } => {
                write!(
                    formatter,
                    "port inventory request has {actual} ports; maximum is {max}"
                )
            }
            Self::QueueFull { actual, max } => write!(
                formatter,
                "port inventory has {actual} queued requests; maximum is {max}"
            ),
            Self::Scan(detail) => write!(
                formatter,
                "port inventory scan failed: {}",
                bounded_sanitized_detail(detail)
            ),
        }
    }
}

impl std::error::Error for PortScanError {}

const MAX_PORT_DETAIL_CHARS: usize = 256;
const MAX_LISTENER_IDENTITY_DISPLAY_CHARS: usize = 2048;

fn bounded_sanitized_detail(detail: &str) -> String {
    let mut sanitized = String::with_capacity(detail.len().min(MAX_PORT_DETAIL_CHARS));
    let mut characters = detail.chars();
    while sanitized.chars().count() < MAX_PORT_DETAIL_CHARS.saturating_sub(1) {
        let Some(character) = characters.next() else {
            return sanitized;
        };
        sanitized.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }
    if characters.next().is_some() {
        sanitized.push('…');
    }
    sanitized
}

fn normalized_ports(ports: impl IntoIterator<Item = u16>) -> Vec<u16> {
    let mut ports = ports.into_iter().collect::<Vec<_>>();
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn listener_identity_display(listeners: &[ListenerIdentity]) -> String {
    let mut display = String::new();
    for (index, listener) in listeners.iter().enumerate() {
        let identity = format!(
            "PID {} (creation {})",
            listener.pid(),
            listener.creation_time_100ns()
        );
        let separator = if index == 0 { "" } else { ", " };
        if display.chars().count() + separator.chars().count() + identity.chars().count()
            > MAX_LISTENER_IDENTITY_DISPLAY_CHARS
        {
            if display.chars().count() < MAX_LISTENER_IDENTITY_DISPLAY_CHARS {
                display.push('…');
            }
            break;
        }
        display.push_str(separator);
        display.push_str(&identity);
    }
    display
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TcpProtocol {
    Tcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TcpAddressFamily {
    Ipv4,
    Ipv6,
}

/// A raw listener-table row before process creation time is captured.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TcpEndpointRecord {
    protocol: TcpProtocol,
    family: TcpAddressFamily,
    bind_address: IpAddr,
    port: u16,
    pid: u32,
}

impl TcpEndpointRecord {
    pub fn tcp(bind_address: IpAddr, port: u16, pid: u32) -> Self {
        Self {
            protocol: TcpProtocol::Tcp,
            family: if bind_address.is_ipv4() {
                TcpAddressFamily::Ipv4
            } else {
                TcpAddressFamily::Ipv6
            },
            bind_address,
            port,
            pid,
        }
    }

    pub fn protocol(&self) -> TcpProtocol {
        self.protocol
    }

    pub fn family(&self) -> TcpAddressFamily {
        self.family
    }

    pub fn bind_address(&self) -> IpAddr {
        self.bind_address
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn is_ipv4(&self) -> bool {
        self.bind_address.is_ipv4()
    }

    pub fn is_ipv6(&self) -> bool {
        self.bind_address.is_ipv6()
    }

    pub fn is_wildcard(&self) -> bool {
        self.bind_address.is_unspecified()
    }
}

/// One listener endpoint after the PID-plus-creation identity was captured.
///
/// Keeping the protocol, family, and bind address is important: an IPv4
/// wildcard and an IPv6 wildcard are separate rows, and a dual-stack port can
/// contain both managed and unrelated endpoint identities.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TcpEndpoint {
    protocol: TcpProtocol,
    family: TcpAddressFamily,
    bind_address: IpAddr,
    port: u16,
    identity: ListenerIdentity,
}

impl TcpEndpoint {
    pub fn tcp(bind_address: IpAddr, port: u16, identity: ListenerIdentity) -> Self {
        Self {
            protocol: TcpProtocol::Tcp,
            family: if bind_address.is_ipv4() {
                TcpAddressFamily::Ipv4
            } else {
                TcpAddressFamily::Ipv6
            },
            bind_address,
            port,
            identity,
        }
    }

    pub fn from_record(record: &TcpEndpointRecord, identity: ListenerIdentity) -> Self {
        Self::tcp(record.bind_address, record.port, identity)
    }

    pub fn protocol(&self) -> TcpProtocol {
        self.protocol
    }

    pub fn family(&self) -> TcpAddressFamily {
        self.family
    }

    pub fn bind_address(&self) -> IpAddr {
        self.bind_address
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn identity(&self) -> ListenerIdentity {
        self.identity.clone()
    }

    pub fn is_ipv4(&self) -> bool {
        self.bind_address.is_ipv4()
    }

    pub fn is_ipv6(&self) -> bool {
        self.bind_address.is_ipv6()
    }

    pub fn is_wildcard(&self) -> bool {
        self.bind_address.is_unspecified()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ListenerIdentity {
    pid: u32,
    creation_time_100ns: u64,
    canonical_executable: Option<PathBuf>,
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
            canonical_executable: None,
        })
    }

    /// Construct an identity with the canonical executable captured while the
    /// listener row was reconciled. A PID-plus-creation match without this
    /// path is intentionally insufficient for managed ownership.
    pub fn with_executable(
        pid: u32,
        creation_time_100ns: u64,
        executable: impl AsRef<Path>,
    ) -> Result<Self, ListenerIdentityError> {
        let base = Self::new(pid, creation_time_100ns)?;
        let executable = std::fs::canonicalize(executable.as_ref()).map_err(|source| {
            ListenerIdentityError::ExecutableCanonicalization {
                path: executable.as_ref().to_path_buf(),
                source,
            }
        })?;
        Ok(Self {
            canonical_executable: Some(executable),
            ..base
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn creation_time_100ns(&self) -> u64 {
        self.creation_time_100ns
    }

    pub fn canonical_executable(&self) -> Option<&Path> {
        self.canonical_executable.as_deref()
    }

    pub fn has_executable_proof(&self) -> bool {
        self.canonical_executable.is_some()
    }

    fn managed_id(&self) -> ManagedProcessId {
        ManagedProcessId::new(self.pid, self.creation_time_100ns)
            .expect("ListenerIdentity validates its managed process identity")
    }
}

#[derive(Debug)]
pub enum ListenerIdentityError {
    ZeroPid,
    ZeroCreationTime,
    ExecutableCanonicalization {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for ListenerIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroPid => write!(formatter, "listener PID must be non-zero"),
            Self::ZeroCreationTime => {
                write!(formatter, "listener creation time must be non-zero")
            }
            Self::ExecutableCanonicalization { source, .. } => write!(
                formatter,
                "could not canonicalize listener executable: {source}"
            ),
        }
    }
}

impl std::error::Error for ListenerIdentityError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortObservation {
    Free,
    Listeners(Arc<[ListenerIdentity]>),
    ProbeError(String),
}

impl PortObservation {
    pub fn from_listeners(mut listeners: Vec<ListenerIdentity>) -> Self {
        listeners.sort_unstable();
        listeners.dedup();
        if listeners.is_empty() {
            Self::Free
        } else {
            Self::Listeners(Arc::from(listeners.into_boxed_slice()))
        }
    }

    pub fn listeners(&self) -> &[ListenerIdentity] {
        match self {
            Self::Free | Self::ProbeError(_) => &[],
            Self::Listeners(listeners) => listeners,
        }
    }

    pub fn listener(&self) -> Option<ListenerIdentity> {
        match self.listeners() {
            [listener] => Some(listener.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortObservationIssue {
    /// The listener table changed during identity capture and must be retried.
    ReconciliationFault(String),
    /// A process identity or table read was denied/unavailable.
    ProbeError(String),
}

impl PortObservationIssue {
    pub fn detail(&self) -> &str {
        match self {
            Self::ReconciliationFault(detail) | Self::ProbeError(detail) => detail,
        }
    }
}

/// An immutable, batched result of one listener-table probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInventorySnapshot {
    observations: Arc<BTreeMap<u16, PortObservation>>,
    endpoints: Arc<BTreeMap<u16, Arc<[TcpEndpoint]>>>,
    issues: Arc<BTreeMap<u16, PortObservationIssue>>,
    requested_ports: Arc<[u16]>,
    observed_at: Instant,
    publication_sequence: u64,
    endpoint_count: usize,
    error_count: usize,
    validation_error: Option<String>,
}

impl PortInventorySnapshot {
    pub fn new(observations: BTreeMap<u16, PortObservation>) -> Self {
        Self::from_parts(
            observations,
            BTreeMap::new(),
            BTreeMap::new(),
            Instant::now(),
        )
    }

    pub fn with_endpoints(
        observations: BTreeMap<u16, PortObservation>,
        endpoints: BTreeMap<u16, Vec<TcpEndpoint>>,
    ) -> Self {
        let endpoints = endpoints
            .into_iter()
            .map(|(port, mut values)| {
                values.sort_unstable();
                values.dedup();
                (port, Arc::from(values.into_boxed_slice()))
            })
            .collect();
        Self::from_parts(observations, endpoints, BTreeMap::new(), Instant::now())
    }

    pub fn from_parts(
        observations: BTreeMap<u16, PortObservation>,
        endpoints: BTreeMap<u16, Arc<[TcpEndpoint]>>,
        issues: BTreeMap<u16, PortObservationIssue>,
        observed_at: Instant,
    ) -> Self {
        let observations = observations
            .into_iter()
            .map(|(port, observation)| {
                let observation = match observation {
                    PortObservation::ProbeError(detail) => {
                        PortObservation::ProbeError(bounded_sanitized_detail(&detail))
                    }
                    other => other,
                };
                (port, observation)
            })
            .collect::<BTreeMap<_, _>>();
        let issues = issues
            .into_iter()
            .map(|(port, issue)| {
                let issue = match issue {
                    PortObservationIssue::ReconciliationFault(detail) => {
                        PortObservationIssue::ReconciliationFault(bounded_sanitized_detail(&detail))
                    }
                    PortObservationIssue::ProbeError(detail) => {
                        PortObservationIssue::ProbeError(bounded_sanitized_detail(&detail))
                    }
                };
                (port, issue)
            })
            .collect::<BTreeMap<_, _>>();
        let requested_ports = normalized_ports(observations.keys().copied());
        let endpoint_count = endpoints.values().map(|values| values.len()).sum();
        let observation_error_count = observations
            .values()
            .filter(|observation| matches!(observation, PortObservation::ProbeError(_)))
            .count();
        let issue_only_error_count = issues
            .iter()
            .filter(|(port, issue)| {
                matches!(
                    issue,
                    PortObservationIssue::ReconciliationFault(_)
                        | PortObservationIssue::ProbeError(_)
                ) && !matches!(observations.get(port), Some(PortObservation::ProbeError(_)))
            })
            .count();
        let error_count = observation_error_count + issue_only_error_count;
        let mut validation_error = None;
        if observations.len() > MAX_PORTS_PER_SCAN {
            validation_error = Some(format!(
                "snapshot contains {} requested ports; maximum is {}",
                observations.len(),
                MAX_PORTS_PER_SCAN
            ));
        }
        if validation_error.is_none() && endpoint_count > MAX_ENDPOINTS_PER_SCAN {
            validation_error = Some(format!(
                "snapshot contains {} endpoint rows; maximum is {}",
                endpoint_count, MAX_ENDPOINTS_PER_SCAN
            ));
        }
        if validation_error.is_none() && error_count > MAX_SCAN_ERRORS {
            validation_error = Some(format!(
                "snapshot contains {} diagnostic errors; maximum is {}",
                error_count, MAX_SCAN_ERRORS
            ));
        }
        for (port, values) in &endpoints {
            if !observations.contains_key(port) {
                validation_error = Some(format!(
                    "endpoint evidence was returned for unrequested port {port}"
                ));
                break;
            }
            if values.iter().any(|endpoint| endpoint.port() != *port) {
                validation_error = Some(format!(
                    "endpoint evidence for port {port} contains a mismatched endpoint"
                ));
                break;
            }
        }
        if validation_error.is_none() {
            for port in issues.keys() {
                if !observations.contains_key(port) {
                    validation_error = Some(format!(
                        "issue evidence was returned for unrequested port {port}"
                    ));
                    break;
                }
            }
        }
        Self {
            observations: Arc::new(observations),
            endpoints: Arc::new(endpoints),
            issues: Arc::new(issues),
            requested_ports: Arc::from(requested_ports.into_boxed_slice()),
            observed_at,
            publication_sequence: 0,
            endpoint_count,
            error_count,
            validation_error,
        }
    }

    pub fn probe_failure(ports: impl IntoIterator<Item = u16>, detail: impl Into<String>) -> Self {
        let detail = bounded_sanitized_detail(&detail.into());
        let observations = normalized_ports(ports)
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

    pub fn endpoints(&self, port: u16) -> &[TcpEndpoint] {
        self.endpoints.get(&port).map_or(&[], Arc::as_ref)
    }

    pub fn issue(&self, port: u16) -> Option<&PortObservationIssue> {
        self.issues.get(&port)
    }

    pub fn issues(&self) -> &BTreeMap<u16, PortObservationIssue> {
        &self.issues
    }

    pub fn requested_ports(&self) -> &[u16] {
        &self.requested_ports
    }

    pub fn observed_at(&self) -> Instant {
        self.observed_at
    }

    pub fn publication_sequence(&self) -> u64 {
        self.publication_sequence
    }

    pub fn endpoint_count(&self) -> usize {
        self.endpoint_count
    }

    pub fn error_count(&self) -> usize {
        self.error_count
    }

    pub fn validation_error(&self) -> Option<&str> {
        self.validation_error.as_deref()
    }

    pub fn is_valid(&self) -> bool {
        self.validation_error.is_none()
    }

    pub fn is_exactly_for(&self, ports: &[u16]) -> bool {
        self.requested_ports.as_ref() == normalized_ports(ports.iter().copied()).as_slice()
    }

    pub fn is_fresh_at(&self, now: Instant, max_age: Duration) -> bool {
        now.checked_duration_since(self.observed_at)
            .is_some_and(|age| age <= max_age)
    }

    pub fn with_observed_at(mut self, observed_at: Instant) -> Self {
        self.observed_at = observed_at;
        self
    }

    pub fn with_publication_sequence(mut self, publication_sequence: u64) -> Self {
        self.publication_sequence = publication_sequence;
        self
    }

    /// Return whether a second listener-table publication is an independent,
    /// still-fresh observation of exactly the same identities. A second read
    /// with the same publication sequence is not independent evidence and a
    /// changed endpoint/identity set must never settle ownership or
    /// externality.
    pub fn same_authoritative_listener_snapshot(
        &self,
        other: &Self,
        observation_time: Instant,
        deadline: Instant,
    ) -> bool {
        observation_time <= deadline
            && self.is_valid()
            && other.is_valid()
            && self.is_exactly_for(other.requested_ports())
            && self.publication_sequence > 0
            && other.publication_sequence > 0
            && self.publication_sequence != other.publication_sequence
            && self.is_fresh_at(observation_time, DEFAULT_FREE_PROOF_MAX_AGE)
            && other.is_fresh_at(observation_time, DEFAULT_FREE_PROOF_MAX_AGE)
            && self.observations == other.observations
            && self.endpoints == other.endpoints
            && self.issues == other.issues
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
    /// The listener identity is a member of the exact managed generation,
    /// but application-level readiness evidence is not available yet.
    ManagedUnready,
    /// A valid registry snapshot proves the listener is outside the managed
    /// generation. This is distinct from an occupied/unverified listener.
    ProvenExternal,
    /// A listener exists, but this projection cannot prove ownership by the
    /// requested resource generation. Control must remain fail-closed.
    Occupied,
    /// The listener table or managed-generation evidence is incomplete,
    /// stale, contradictory, or otherwise not safe to classify. This state
    /// is never rendered as proven external ownership.
    Unknown,
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
    pub listeners: Arc<[ListenerIdentity]>,
    pub error: Option<String>,
}

impl PortStatus {
    pub fn kind(&self) -> PortStatusKind {
        self.kind
    }

    pub fn listener(&self) -> Option<ListenerIdentity> {
        match self.listeners.as_ref() {
            [listener] => Some(listener.clone()),
            _ => None,
        }
    }

    pub fn listeners(&self) -> &[ListenerIdentity] {
        &self.listeners
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedProcessSnapshotValidity {
    Valid,
    Stale,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryMembershipSnapshot {
    membership_revision: u64,
    observation_sequence: u64,
    observed_at: Instant,
    max_age: Duration,
    validity: ManagedProcessSnapshotValidity,
    detail: Option<String>,
}

impl RegistryMembershipSnapshot {
    pub(crate) fn valid(
        membership_revision: u64,
        observation_sequence: u64,
        observed_at: Instant,
        max_age: Duration,
    ) -> Self {
        Self {
            membership_revision,
            observation_sequence,
            observed_at,
            max_age,
            validity: ManagedProcessSnapshotValidity::Valid,
            detail: None,
        }
    }

    pub(crate) fn stale(
        membership_revision: u64,
        observation_sequence: u64,
        observed_at: Instant,
    ) -> Self {
        Self {
            membership_revision,
            observation_sequence,
            observed_at,
            max_age: Duration::ZERO,
            validity: ManagedProcessSnapshotValidity::Stale,
            detail: Some("membership observation is stale".to_string()),
        }
    }

    pub(crate) fn failed(
        membership_revision: u64,
        observation_sequence: u64,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            membership_revision,
            observation_sequence,
            observed_at: Instant::now(),
            max_age: Duration::ZERO,
            validity: ManagedProcessSnapshotValidity::Failed,
            detail: Some(bounded_sanitized_detail(&detail.into())),
        }
    }

    pub fn membership_revision(&self) -> u64 {
        self.membership_revision
    }

    pub fn observation_sequence(&self) -> u64 {
        self.observation_sequence
    }

    pub fn observed_at(&self) -> Instant {
        self.observed_at
    }

    pub fn validity(&self) -> ManagedProcessSnapshotValidity {
        self.validity
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    pub fn is_fresh_at(&self, now: Instant) -> bool {
        self.validity == ManagedProcessSnapshotValidity::Valid
            && self.membership_revision > 0
            && self.observation_sequence > 0
            && now
                .checked_duration_since(self.observed_at)
                .is_some_and(|age| age <= self.max_age)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedResourceSnapshot {
    fence: ManagedProcessFence,
    state: ManagedProcessState,
    members: Arc<[ManagedProcessIdentity]>,
    membership: RegistryMembershipSnapshot,
}

impl ManagedResourceSnapshot {
    pub(crate) fn new(
        fence: ManagedProcessFence,
        state: ManagedProcessState,
        mut members: Vec<ManagedProcessIdentity>,
        membership: RegistryMembershipSnapshot,
    ) -> Self {
        members.sort_unstable_by_key(|identity| {
            (
                identity.id().pid(),
                identity.id().creation_time_100ns(),
                identity.canonical_executable().to_path_buf(),
            )
        });
        members.dedup();
        Self {
            fence,
            state,
            members: Arc::from(members.into_boxed_slice()),
            membership,
        }
    }

    pub fn resource(&self) -> ResourceFence {
        self.fence.resource()
    }

    pub fn fence(&self) -> &ManagedProcessFence {
        &self.fence
    }

    pub fn owner(&self) -> ProcessOwner {
        self.fence.owner()
    }

    pub fn root(&self) -> &ManagedProcessIdentity {
        self.fence.root()
    }

    pub fn resource_id(&self) -> ResourceId {
        self.fence.resource().resource_id
    }

    pub fn generation(&self) -> u64 {
        self.fence.resource().runtime_generation
    }

    pub fn state(&self) -> ManagedProcessState {
        self.state
    }

    pub fn member_identities(&self) -> &[ManagedProcessIdentity] {
        &self.members
    }

    pub fn membership_revision(&self) -> u64 {
        self.membership.membership_revision()
    }

    pub fn observation_sequence(&self) -> u64 {
        self.membership.observation_sequence()
    }

    pub fn observed_at(&self) -> Instant {
        self.membership.observed_at()
    }

    pub fn validity(&self) -> ManagedProcessSnapshotValidity {
        self.membership.validity()
    }

    pub fn membership(&self) -> &RegistryMembershipSnapshot {
        &self.membership
    }

    pub fn is_fresh_at(&self, now: Instant) -> bool {
        self.membership.is_fresh_at(now) && self.structure_is_valid()
    }

    /// A path-free binding for a wire authority. The canonical executable is
    /// included in the digest without exposing it, so a PID/creation/fence
    /// shaped like the live one cannot be accepted unless it came from the
    /// same exact root and member executable set.
    pub fn authority_fingerprint(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.resource().resource_id.hash(&mut hasher);
        self.resource().runtime_generation.hash(&mut hasher);
        self.owner().hash(&mut hasher);
        self.root().id().pid().hash(&mut hasher);
        self.root().id().creation_time_100ns().hash(&mut hasher);
        self.root()
            .canonical_executable()
            .as_os_str()
            .hash(&mut hasher);
        self.state.hash(&mut hasher);
        for member in self.member_identities() {
            member.id().pid().hash(&mut hasher);
            member.id().creation_time_100ns().hash(&mut hasher);
            member.canonical_executable().as_os_str().hash(&mut hasher);
        }
        self.membership_revision().hash(&mut hasher);
        self.observation_sequence().hash(&mut hasher);
        hasher.finish()
    }

    /// Compare the identity-bearing registry observation that accompanied a
    /// listener scan with a second authoritative registry read. The wall
    /// clock may advance between reads, so freshness timestamps are checked
    /// separately; the fence, lifecycle, member identities, and registry
    /// observation sequence must remain the same before ownership or
    /// externality can be projected.
    pub fn same_authoritative_membership(
        &self,
        other: &Self,
        observation_time: Instant,
        deadline: Instant,
    ) -> bool {
        observation_time <= deadline
            && self.is_fresh_at(observation_time)
            && other.is_fresh_at(observation_time)
            && self.fence == other.fence
            && self.state == other.state
            && self.members == other.members
            && self.membership.membership_revision() == other.membership.membership_revision()
            && self.membership.observation_sequence() == other.membership.observation_sequence()
            && self.membership.validity() == other.membership.validity()
    }

    fn structure_is_valid(&self) -> bool {
        self.members
            .iter()
            .any(|member| member.matches_root(self.fence.root()))
    }

    fn owns(&self, listener: &ListenerIdentity) -> bool {
        let Some(executable) = listener.canonical_executable() else {
            return false;
        };
        self.members.iter().any(|member| {
            member.id() == listener.managed_id() && member.canonical_executable() == executable
        })
    }

    fn shares_pid(&self, listener: &ListenerIdentity) -> bool {
        self.members
            .iter()
            .any(|member| member.id().pid() == listener.pid())
    }

    fn ownership_confident_at(&self, now: Instant) -> bool {
        self.state == ManagedProcessState::Running && self.is_fresh_at(now)
    }
}

/// Opaque host authority issued only from a live process-registry observation.
///
/// The inner snapshot is intentionally private and this capability is not
/// cloneable. Host publication may retain it behind an `Arc`, but callers can
/// never construct a capability from a DTO or an arbitrary snapshot shape.
#[derive(Debug)]
pub(crate) struct ManagedResourceCapability {
    snapshot: ManagedResourceSnapshot,
}

impl ManagedResourceCapability {
    pub(crate) fn snapshot(&self) -> &ManagedResourceSnapshot {
        &self.snapshot
    }
}

/// Issue one opaque authority from the current registry/Job observation. This
/// is the only production constructor for [`ManagedResourceCapability`].
pub(crate) fn issue_managed_resource_capability<J: crate::process::registry::JobMembership>(
    registry: &mut ProcessRegistry<J>,
    resource: ResourceFence,
    observed_at: Instant,
    max_age: Duration,
) -> Option<ManagedResourceCapability> {
    registry
        .managed_resource_snapshot(resource, observed_at, max_age)
        .map(|snapshot| ManagedResourceCapability { snapshot })
}

#[cfg(test)]
pub(crate) fn test_capability_from_snapshot(
    snapshot: ManagedResourceSnapshot,
) -> ManagedResourceCapability {
    ManagedResourceCapability { snapshot }
}

/// Classification-only result for one listener observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortAuthority {
    Managed,
    ProvenExternal,
    Unknown,
    ProbeError,
    Free,
}

fn classify_listener_authority(
    listeners: &[ListenerIdentity],
    managed: Option<&ManagedResourceSnapshot>,
    now: Instant,
    deadline: Instant,
) -> PortAuthority {
    if now > deadline {
        return PortAuthority::Unknown;
    }
    let Some(managed) = managed else {
        if listeners.is_empty() {
            return PortAuthority::Free;
        }
        // An executable path proves what the listener is, not that it is
        // outside a live DevManager generation. Without the registry's
        // current managed/leaked/transitioning snapshot, occupied is only an
        // observation and must remain Unknown.
        return PortAuthority::Unknown;
    };
    if !managed.is_fresh_at(now) || managed.state != ManagedProcessState::Running {
        return PortAuthority::Unknown;
    }
    if listeners.is_empty() {
        return PortAuthority::Free;
    }
    if listeners
        .iter()
        .any(|listener| !listener.has_executable_proof())
    {
        return PortAuthority::Unknown;
    }

    let any_owned = listeners.iter().any(|listener| managed.owns(listener));
    let all_owned = listeners.iter().all(|listener| managed.owns(listener));
    if managed.ownership_confident_at(now) {
        if all_owned {
            PortAuthority::Managed
        } else if any_owned
            || listeners
                .iter()
                .any(|listener| managed.shares_pid(listener))
        {
            // A PID reuse or a managed/external mixture is a reconciliation
            // fault. It is not safe to paint it green or external blue.
            PortAuthority::Unknown
        } else {
            PortAuthority::ProvenExternal
        }
    } else if any_owned
        || listeners
            .iter()
            .any(|listener| managed.shares_pid(listener))
    {
        PortAuthority::Unknown
    } else {
        PortAuthority::ProvenExternal
    }
}

pub fn classify_port_authority(
    observation: &PortObservation,
    managed: Option<&ManagedResourceSnapshot>,
) -> PortAuthority {
    let now = Instant::now();
    classify_port_authority_at(
        observation,
        managed,
        now,
        now.checked_add(DEFAULT_FREE_PROOF_MAX_AGE).unwrap_or(now),
    )
}

/// Classify an observation against an explicit observation clock and
/// deadline. Callers that hold a scan deadline must pass it through this
/// boundary; using a fresh `Instant::now()` in the projection would otherwise
/// allow a callback to publish evidence after its admission window expired.
pub fn classify_port_authority_at(
    observation: &PortObservation,
    managed: Option<&ManagedResourceSnapshot>,
    observed_at: Instant,
    deadline: Instant,
) -> PortAuthority {
    let now = Instant::now();
    if now > deadline
        || now
            .checked_duration_since(observed_at)
            .is_none_or(|age| age > DEFAULT_FREE_PROOF_MAX_AGE)
    {
        return PortAuthority::Unknown;
    }
    match observation {
        PortObservation::Free => classify_listener_authority(&[], managed, now, deadline),
        PortObservation::ProbeError(_) => PortAuthority::ProbeError,
        PortObservation::Listeners(listeners) => {
            classify_listener_authority(listeners, managed, now, deadline)
        }
    }
}

pub fn classify_port_authority_from_snapshot(
    target: &PortTarget,
    snapshot: &PortInventorySnapshot,
    managed: Option<&ManagedResourceSnapshot>,
) -> PortAuthority {
    let now = Instant::now();
    classify_port_authority_from_snapshot_at(
        target,
        snapshot,
        managed,
        now,
        now.checked_add(DEFAULT_FREE_PROOF_MAX_AGE).unwrap_or(now),
    )
}

pub fn classify_port_authority_from_snapshot_at(
    target: &PortTarget,
    snapshot: &PortInventorySnapshot,
    managed: Option<&ManagedResourceSnapshot>,
    now: Instant,
    deadline: Instant,
) -> PortAuthority {
    let Some(observation) = snapshot.observation(target.port) else {
        return PortAuthority::ProbeError;
    };
    if now > deadline || !snapshot.is_fresh_at(now, DEFAULT_FREE_PROOF_MAX_AGE) {
        return PortAuthority::Unknown;
    }
    match snapshot.issue(target.port) {
        Some(PortObservationIssue::ProbeError(_))
        | Some(PortObservationIssue::ReconciliationFault(_)) => return PortAuthority::Unknown,
        None => {}
    }
    if !snapshot.is_valid() {
        return PortAuthority::Unknown;
    }
    if matches!(observation, PortObservation::ProbeError(_)) {
        return PortAuthority::ProbeError;
    }
    if let Some(managed) = managed {
        if managed.resource() != target.resource {
            return PortAuthority::Unknown;
        }
    }
    let listeners = if snapshot.endpoints(target.port).is_empty() {
        observation.listeners().to_vec()
    } else {
        let mut endpoint_listeners = snapshot
            .endpoints(target.port)
            .iter()
            .map(|endpoint| endpoint.identity())
            .collect::<Vec<_>>();
        endpoint_listeners.sort_unstable();
        endpoint_listeners.dedup();
        let mut observed_listeners = observation.listeners().to_vec();
        observed_listeners.sort_unstable();
        if endpoint_listeners != observed_listeners {
            return PortAuthority::Unknown;
        }
        endpoint_listeners
    };
    classify_listener_authority(&listeners, managed, now, deadline)
}

/// Classify only after two registry observations agree on the exact managed
/// generation and membership. A listener table can be internally reconciled
/// while the registry membership changes immediately afterwards; that
/// cross-source race is not safe to paint external or managed.
pub fn classify_port_authority_from_snapshot_with_membership_reconciliation_at(
    target: &PortTarget,
    snapshot: &PortInventorySnapshot,
    managed: Option<&ManagedResourceSnapshot>,
    reconciled_managed: Option<&ManagedResourceSnapshot>,
    now: Instant,
    deadline: Instant,
) -> PortAuthority {
    let membership_agrees = match (managed, reconciled_managed) {
        (None, None) => true,
        (Some(first), Some(second)) => first.same_authoritative_membership(second, now, deadline),
        (None, Some(_)) | (Some(_), None) => false,
    };
    if !membership_agrees {
        return PortAuthority::Unknown;
    }
    classify_port_authority_from_snapshot_at(target, snapshot, managed, now, deadline)
}

/// Reconcile listener identity and managed membership independently after the
/// first scan. This is the only projection seam that can settle an occupied
/// port as Managed or ProvenExternal: both listener publications and both
/// registry publications must be current and agree on their exact fences.
pub fn classify_port_authority_from_two_snapshots_at(
    target: &PortTarget,
    first: &PortInventorySnapshot,
    second: &PortInventorySnapshot,
    managed: Option<&ManagedResourceSnapshot>,
    reconciled_managed: Option<&ManagedResourceSnapshot>,
    now: Instant,
    deadline: Instant,
) -> PortAuthority {
    if !first.same_authoritative_listener_snapshot(second, now, deadline) {
        return PortAuthority::Unknown;
    }
    classify_port_authority_from_snapshot_with_membership_reconciliation_at(
        target,
        first,
        managed,
        reconciled_managed,
        now,
        deadline,
    )
}

fn status_for_authority(
    target: &PortTarget,
    listeners: Arc<[ListenerIdentity]>,
    authority: PortAuthority,
    managed: Option<&ManagedResourceSnapshot>,
) -> PortStatus {
    let kind = match authority {
        PortAuthority::Free => PortStatusKind::Stopped,
        PortAuthority::ProbeError => PortStatusKind::ProbeError,
        PortAuthority::ProvenExternal => PortStatusKind::ProvenExternal,
        PortAuthority::Unknown => PortStatusKind::Unknown,
        PortAuthority::Managed => {
            if managed.is_some_and(|resource| {
                resource.state() == ManagedProcessState::Running
                    && target.health == ManagedPortHealth::Ready
            }) {
                PortStatusKind::ManagedHealthy
            } else {
                PortStatusKind::ManagedUnready
            }
        }
    };
    PortStatus {
        port: target.port,
        resource: target.resource,
        kind,
        listeners,
        error: None,
    }
}

pub fn project_port_status(
    target: &PortTarget,
    observation: &PortObservation,
    managed: Option<&ManagedResourceSnapshot>,
) -> PortStatus {
    let now = Instant::now();
    project_port_status_at(
        target,
        observation,
        managed,
        now,
        now.checked_add(DEFAULT_FREE_PROOF_MAX_AGE).unwrap_or(now),
    )
}

pub fn project_port_status_at(
    target: &PortTarget,
    observation: &PortObservation,
    managed: Option<&ManagedResourceSnapshot>,
    observed_at: Instant,
    deadline: Instant,
) -> PortStatus {
    let now = Instant::now();
    if managed.is_some_and(|resource| resource.resource() != target.resource) {
        return PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::Unknown,
            listeners: Arc::from(observation.listeners()),
            error: Some(bounded_sanitized_detail(
                "managed resource fence does not match the requested port target",
            )),
        };
    }
    let managed = managed.filter(|resource| resource.resource() == target.resource);
    if managed.is_some_and(|resource| {
        resource.resource() == target.resource && resource.state() == ManagedProcessState::Starting
    }) {
        return PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::Starting,
            listeners: Arc::from(observation.listeners()),
            error: match observation {
                PortObservation::ProbeError(detail) => Some(bounded_sanitized_detail(detail)),
                PortObservation::Free | PortObservation::Listeners(_) => None,
            },
        };
    }
    let evidence_fresh = now <= deadline
        && now
            .checked_duration_since(observed_at)
            .is_some_and(|age| age <= DEFAULT_FREE_PROOF_MAX_AGE);
    if !evidence_fresh {
        return PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::Unknown,
            listeners: Arc::from(observation.listeners()),
            error: Some(bounded_sanitized_detail(
                "port observation is stale or its projection deadline expired",
            )),
        };
    }
    match observation {
        PortObservation::Free => {
            let authority = classify_port_authority_at(observation, managed, observed_at, deadline);
            status_for_authority(target, Arc::from([]), authority, managed)
        }
        PortObservation::ProbeError(detail) => PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::ProbeError,
            listeners: Arc::from([]),
            error: Some(bounded_sanitized_detail(detail)),
        },
        PortObservation::Listeners(listeners) => {
            let authority = classify_listener_authority(listeners, managed, now, deadline);
            status_for_authority(target, listeners.clone(), authority, managed)
        }
    }
}

pub fn project_port_status_from_snapshot(
    target: &PortTarget,
    snapshot: &PortInventorySnapshot,
    managed: Option<&ManagedResourceSnapshot>,
) -> PortStatus {
    let now = Instant::now();
    project_port_status_from_snapshot_at(
        target,
        snapshot,
        managed,
        now,
        now.checked_add(DEFAULT_FREE_PROOF_MAX_AGE).unwrap_or(now),
    )
}

pub fn project_port_status_from_snapshot_at(
    target: &PortTarget,
    snapshot: &PortInventorySnapshot,
    managed: Option<&ManagedResourceSnapshot>,
    now: Instant,
    deadline: Instant,
) -> PortStatus {
    if managed.is_some_and(|resource| resource.resource() != target.resource) {
        let listeners = snapshot
            .observation(target.port)
            .map_or_else(Vec::new, |observation| observation.listeners().to_vec());
        return PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::Unknown,
            listeners: Arc::from(listeners.into_boxed_slice()),
            error: Some(bounded_sanitized_detail(
                "managed resource fence does not match the requested port target",
            )),
        };
    }
    let Some(observation) = snapshot.observation(target.port) else {
        return PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::Unknown,
            listeners: Arc::from([]),
            error: Some(bounded_sanitized_detail(
                "port was not included in the cached inventory snapshot",
            )),
        };
    };
    let starting = managed.is_some_and(|resource| {
        resource.resource() == target.resource && resource.state() == ManagedProcessState::Starting
    });
    if starting
        && (snapshot.issue(target.port).is_some()
            || matches!(observation, PortObservation::ProbeError(_)))
    {
        return PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::Starting,
            listeners: Arc::from(observation.listeners()),
            error: snapshot
                .issue(target.port)
                .map(|issue| bounded_sanitized_detail(issue.detail()))
                .or_else(|| match observation {
                    PortObservation::ProbeError(detail) => Some(bounded_sanitized_detail(detail)),
                    PortObservation::Free | PortObservation::Listeners(_) => None,
                }),
        };
    }
    if now > deadline || !snapshot.is_fresh_at(now, DEFAULT_FREE_PROOF_MAX_AGE) {
        return PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::Unknown,
            listeners: Arc::from(observation.listeners()),
            error: Some(bounded_sanitized_detail(
                "port inventory observation is stale or its projection deadline expired",
            )),
        };
    }
    if starting {
        return PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::Starting,
            listeners: Arc::from(observation.listeners()),
            error: None,
        };
    }
    if let Some(issue) = snapshot.issue(target.port) {
        return PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::Unknown,
            listeners: Arc::from(observation.listeners()),
            error: Some(bounded_sanitized_detail(issue.detail())),
        };
    }
    if !snapshot.is_valid() {
        return PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::Unknown,
            listeners: Arc::from(observation.listeners()),
            error: snapshot.validation_error().map(bounded_sanitized_detail),
        };
    }
    let authority =
        classify_port_authority_from_snapshot_at(target, snapshot, managed, now, deadline);
    status_for_authority(
        target,
        Arc::from(observation.listeners()),
        authority,
        managed,
    )
}

/// Projection boundary for callers that can obtain a second authoritative
/// registry membership snapshot after listener enumeration. If no matching
/// second observation exists, fail closed with the listener evidence retained
/// for diagnostics but without a green/blue authority claim.
pub fn project_port_status_from_snapshot_with_membership_reconciliation_at(
    target: &PortTarget,
    snapshot: &PortInventorySnapshot,
    managed: Option<&ManagedResourceSnapshot>,
    reconciled_managed: Option<&ManagedResourceSnapshot>,
    now: Instant,
    deadline: Instant,
) -> PortStatus {
    let membership_agrees = match (managed, reconciled_managed) {
        (None, None) => true,
        (Some(first), Some(second)) => first.same_authoritative_membership(second, now, deadline),
        (None, Some(_)) | (Some(_), None) => false,
    };
    if !membership_agrees {
        let listeners = snapshot
            .observation(target.port)
            .map_or_else(Vec::new, |observation| observation.listeners().to_vec());
        return PortStatus {
            port: target.port,
            resource: target.resource,
            kind: PortStatusKind::Unknown,
            listeners: Arc::from(listeners.into_boxed_slice()),
            error: Some(bounded_sanitized_detail(
                "managed membership changed during port authority reconciliation",
            )),
        };
    }
    project_port_status_from_snapshot_at(target, snapshot, managed, now, deadline)
}

/// Project a typed status only after an independently fresh listener
/// publication and an independently fresh, equal registry membership
/// publication have both been captured.
pub fn project_port_status_from_two_snapshots_at(
    target: &PortTarget,
    first: &PortInventorySnapshot,
    second: &PortInventorySnapshot,
    managed: Option<&ManagedResourceSnapshot>,
    reconciled_managed: Option<&ManagedResourceSnapshot>,
    now: Instant,
    deadline: Instant,
) -> PortStatus {
    let authority = classify_port_authority_from_two_snapshots_at(
        target,
        first,
        second,
        managed,
        reconciled_managed,
        now,
        deadline,
    );
    let listeners = first
        .observation(target.port)
        .map_or_else(Vec::new, |observation| observation.listeners().to_vec());
    status_for_authority(
        target,
        Arc::from(listeners.into_boxed_slice()),
        authority,
        managed,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortStartError {
    NotScanned {
        port: u16,
    },
    NotExactSnapshot {
        port: u16,
    },
    StaleProof {
        port: u16,
    },
    UnsequencedProof {
        port: u16,
    },
    ReservationConflict {
        port: u16,
    },
    ProbeFailed {
        port: u16,
        detail: String,
    },
    Occupied {
        port: u16,
        listener: ListenerIdentity,
    },
    OccupiedAmbiguous {
        port: u16,
        listeners: Arc<[ListenerIdentity]>,
    },
}

impl fmt::Display for PortStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotScanned { port } => {
                write!(formatter, "port {port} has no cached listener observation")
            }
            Self::NotExactSnapshot { port } => write!(
                formatter,
                "port {port} has no exact single-port free proof; a partial snapshot cannot authorize launch"
            ),
            Self::StaleProof { port } => write!(
                formatter,
                "port {port} has a stale free proof; a fresh listener observation is required"
            ),
            Self::UnsequencedProof { port } => write!(
                formatter,
                "port {port} has an unsequenced free proof; a published inventory snapshot is required"
            ),
            Self::ReservationConflict { port } => write!(
                formatter,
                "port {port} already has an active DevManager start reservation"
            ),
            Self::ProbeFailed { port, detail } => {
                write!(
                    formatter,
                    "could not establish whether port {port} is free: {}",
                    bounded_sanitized_detail(detail)
                )
            }
            Self::Occupied { port, listener } => write!(
                formatter,
                "port {port} is occupied; ownership is unverified (PID {} (creation {}))",
                listener.pid(),
                listener.creation_time_100ns()
            ),
            Self::OccupiedAmbiguous { port, listeners } => write!(
                formatter,
                "port {port} is occupied by multiple listeners; ownership is unverified. Captured identities: {}",
                listener_identity_display(listeners)
            ),
        }
    }
}

impl std::error::Error for PortStartError {}

/// Admit a managed launch only when the immutable scan proved the port free.
///
/// This function has no process-control capability. In particular, a
/// listener is reported with evidence and is never assigned, adopted,
/// signaled, or killed as part of the rejection. The proof must be a fresh,
/// exact single-port snapshot; a later bind race is left to the launch path.
pub fn ensure_managed_start_allowed(
    snapshot: &PortInventorySnapshot,
    port: u16,
) -> Result<(), PortStartError> {
    let now = Instant::now();
    ensure_managed_start_allowed_at(
        snapshot,
        port,
        now,
        now.checked_add(DEFAULT_FREE_PROOF_MAX_AGE).unwrap_or(now),
    )
}

pub fn ensure_managed_start_allowed_at(
    snapshot: &PortInventorySnapshot,
    port: u16,
    now: Instant,
    deadline: Instant,
) -> Result<(), PortStartError> {
    if !snapshot.is_valid() {
        return Err(PortStartError::ProbeFailed {
            port,
            detail: snapshot
                .validation_error()
                .unwrap_or("port inventory snapshot failed validation")
                .to_string(),
        });
    }
    match snapshot.observation(port) {
        None => Err(PortStartError::NotScanned { port }),
        Some(_) if !snapshot.is_exactly_for(&[port]) => {
            Err(PortStartError::NotExactSnapshot { port })
        }
        Some(_) if now > deadline || !snapshot.is_fresh_at(now, DEFAULT_FREE_PROOF_MAX_AGE) => {
            Err(PortStartError::StaleProof { port })
        }
        Some(_) if snapshot.issue(port).is_some() => Err(PortStartError::ProbeFailed {
            port,
            detail: snapshot
                .issue(port)
                .expect("issue checked above")
                .detail()
                .to_string(),
        }),
        Some(PortObservation::Free)
            if snapshot.endpoints(port).is_empty() && snapshot.publication_sequence() > 0 =>
        {
            Ok(())
        }
        Some(PortObservation::Free) if snapshot.publication_sequence() == 0 => {
            Err(PortStartError::UnsequencedProof { port })
        }
        Some(PortObservation::Free) => Err(PortStartError::ProbeFailed {
            port,
            detail: "free observation retained endpoint evidence".to_string(),
        }),
        Some(PortObservation::ProbeError(detail)) => Err(PortStartError::ProbeFailed {
            port,
            detail: detail.clone(),
        }),
        Some(PortObservation::Listeners(listeners)) => {
            if let Some(issue) = snapshot.issue(port) {
                return Err(PortStartError::ProbeFailed {
                    port,
                    detail: issue.detail().to_string(),
                });
            }
            match listeners.as_ref() {
                [listener] => Err(PortStartError::Occupied {
                    port,
                    listener: listener.clone(),
                }),
                _ => Err(PortStartError::OccupiedAmbiguous {
                    port,
                    listeners: listeners.clone(),
                }),
            }
        }
    }
}

/// Invoke a managed launch only after the immutable inventory proved the port
/// free. The callback is never evaluated for a missing, occupied, ambiguous,
/// stale, partial, or failed observation.
pub fn launch_if_port_free<T>(
    snapshot: &PortInventorySnapshot,
    port: u16,
    launch: impl FnOnce() -> T,
) -> Result<T, PortStartError> {
    ensure_managed_start_allowed(snapshot, port)?;
    Ok(launch())
}

/// Admit a launch only after the owner has performed an exact, current
/// revalidation immediately before binding/spawning. The revalidation closure
/// belongs to the start owner; this module never pretends that a cached scan
/// is an atomic OS bind operation.
pub fn launch_if_port_free_with_revalidation<T>(
    snapshot: &PortInventorySnapshot,
    port: u16,
    revalidate: impl FnOnce() -> Result<(), PortStartError>,
    launch: impl FnOnce() -> T,
) -> Result<T, PortStartError> {
    ensure_managed_start_allowed(snapshot, port)?;
    revalidate()?;
    Ok(launch())
}

#[cfg(test)]
mod authority_tests {
    use super::*;
    use crate::domain::id::ResourceId;

    fn listener(pid: u32, creation: u64) -> ListenerIdentity {
        ListenerIdentity::with_executable(pid, creation, std::env::current_exe().unwrap())
            .expect("test executable is canonicalizable")
    }

    #[test]
    fn occupied_listener_without_authoritative_registry_is_unknown() {
        let port = 31_741;
        let identity = listener(41, 9);
        let snapshot = PortInventorySnapshot::from_parts(
            BTreeMap::from([(port, PortObservation::from_listeners(vec![identity]))]),
            BTreeMap::new(),
            BTreeMap::new(),
            Instant::now(),
        );
        let now = Instant::now();
        let target = PortTarget::new(
            port,
            ResourceFence::new(ResourceId::new(), 1),
            ManagedPortHealth::Ready,
        );

        assert_eq!(
            classify_port_authority_from_snapshot_with_membership_reconciliation_at(
                &target,
                &snapshot,
                None,
                None,
                now,
                now + DEFAULT_FREE_PROOF_MAX_AGE,
            ),
            PortAuthority::Unknown,
            "PID/executable evidence cannot prove externality without a current registry snapshot"
        );
    }

    #[test]
    fn membership_reconciliation_rejects_stale_second_snapshot() {
        let port = 31_742;
        let root = ManagedProcessIdentity::new(
            ManagedProcessId::new(42, 10).unwrap(),
            std::env::current_exe().unwrap(),
        )
        .unwrap();
        let resource = ResourceFence::new(ResourceId::new(), 3);
        let fence = ManagedProcessFence::new(resource, ProcessOwner::Host, root.clone());
        let first = ManagedResourceSnapshot::new(
            fence.clone(),
            ManagedProcessState::Running,
            vec![root.clone()],
            RegistryMembershipSnapshot::valid(7, 11, Instant::now(), Duration::from_secs(30)),
        );
        let second = ManagedResourceSnapshot::new(
            fence,
            ManagedProcessState::Running,
            vec![root.clone()],
            RegistryMembershipSnapshot::stale(7, 11, Instant::now() - Duration::from_secs(60)),
        );
        let observation_time = Instant::now();
        let deadline = observation_time + DEFAULT_MEMBERSHIP_MAX_AGE;
        assert!(!first.same_authoritative_membership(&second, observation_time, deadline));
        assert!(!first.same_authoritative_membership(
            &first,
            observation_time,
            observation_time - Duration::from_millis(1),
        ));

        let identity = listener(42, 10);
        let snapshot = PortInventorySnapshot::from_parts(
            BTreeMap::from([(port, PortObservation::from_listeners(vec![identity]))]),
            BTreeMap::new(),
            BTreeMap::new(),
            Instant::now(),
        );
        let now = Instant::now();
        let target = PortTarget::new(port, resource, ManagedPortHealth::Ready);
        assert_eq!(
            classify_port_authority_from_snapshot_with_membership_reconciliation_at(
                &target,
                &snapshot,
                Some(&first),
                Some(&second),
                now,
                now + DEFAULT_FREE_PROOF_MAX_AGE,
            ),
            PortAuthority::Unknown
        );
    }

    #[test]
    fn second_listener_snapshot_must_be_fresh_and_identity_equal() {
        let port = 31_743;
        let now = Instant::now();
        let first_listener = listener(43, 11);
        let second_listener = listener(43, 12);
        let first = PortInventorySnapshot::from_parts(
            BTreeMap::from([(
                port,
                PortObservation::from_listeners(vec![first_listener.clone()]),
            )]),
            BTreeMap::new(),
            BTreeMap::new(),
            now,
        )
        .with_publication_sequence(1);
        let changed = PortInventorySnapshot::from_parts(
            BTreeMap::from([(port, PortObservation::from_listeners(vec![second_listener]))]),
            BTreeMap::new(),
            BTreeMap::new(),
            now,
        )
        .with_publication_sequence(2);
        let deadline = now + DEFAULT_FREE_PROOF_MAX_AGE;
        assert!(!first.same_authoritative_listener_snapshot(&changed, now, deadline));
        assert!(!first.same_authoritative_listener_snapshot(
            &first,
            now,
            now - Duration::from_millis(1),
        ));

        let stale = PortInventorySnapshot::from_parts(
            BTreeMap::from([(port, PortObservation::from_listeners(vec![first_listener]))]),
            BTreeMap::new(),
            BTreeMap::new(),
            now - Duration::from_secs(60),
        )
        .with_publication_sequence(3);
        assert!(!first.same_authoritative_listener_snapshot(&stale, now, deadline));
    }
}
