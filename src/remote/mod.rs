mod access_log;
pub(crate) mod blocking_work;
pub(crate) use blocking_work::{BackgroundWorkStop, RemoteBackgroundWork};
mod client_pool;
pub mod presentation;
mod transport;
pub mod web;

pub use access_log::{RemoteAccessActivityEvent, RemoteAccessActivityKind, RemoteAccessSource};
pub use client_pool::RemoteClientPool;
pub(crate) use web::{validate_or_bind_connect_peer, ConnectPeerLease};
pub use web::{
    ConnectPeerPin, ConnectPeerPublicKey, ConnectPeerTrustError, PairedWebClient, WebConfig,
    WebListenerHandle, CONNECT_PEER_PUBLIC_KEY_BYTES, CONNECT_PEER_PUBLIC_KEY_HEX_CHARS,
    MAX_CONNECT_PEER_PINS, MAX_PAIRED_COOKIE_CLIENT_ID_BYTES,
};

use presentation::{
    SemanticAdapterHealth, SemanticAttention, SemanticEvent, SemanticEventDraft, SemanticEventKind,
    SemanticJournalStore, SemanticReplay, SemanticSessionMetadata, SemanticSource,
    StableSessionKey,
};
use web::bridge::{BrowserOutboundSender, WebConnectionTombstone};
use web::input_executor::WebInputExecutor;
use web::lease::{ControllerRequest, ControllerTarget, WebControlState};
use web::request_executor::WebRequestExecutor;

use crate::domain::operation::ResourceFence;
use crate::git::command::GitHostBinding;
use crate::git::git_service::{
    AiCommitMessage, DeviceCodeResponse, GitBranch, GitDiffResult, GitLogEntry, GitStatusResult,
};
use crate::models::{
    PortStatus, Project, ProjectFolder, RootScanEntry, RunCommand, SSHConnection, ScanResult,
    Settings, TabType,
};
use crate::persistence::{self, PersistenceError};
use crate::process::ports::{
    ManagedResourceCapability, PortStatus as RichPortStatus, PortStatusKind as RichPortStatusKind,
};
use crate::state::{
    AppState, RuntimeState, SessionDimensions, SessionKind, SessionRuntimeState, SessionStatus,
};
use crate::terminal::session::{
    TerminalModeSnapshot, TerminalReplica, TerminalScreenSnapshot, TerminalSearchMatch,
    TerminalSessionView,
};
use rmp_serde::{decode::from_slice as from_messagepack_slice, encode::to_vec_named};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock, RwLock, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;

pub const PROTOCOL_VERSION: u32 = 5;
const REMOTE_FILE_NAME: &str = "remote.json";
const SNAPSHOT_BROADCAST_INTERVAL: Duration = Duration::from_millis(33);
const IDLE_BROADCAST_INTERVAL: Duration = Duration::from_millis(250);
const PENDING_BOOTSTRAP_RETRY_INTERVAL: Duration = Duration::from_millis(250);
pub(in crate::remote) const REMOTE_WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
const REMOTE_CALLBACK_TIMEOUT: Duration = Duration::from_millis(500);
// Native outbound traffic is channel-backed while readiness waits observe only
// the socket. Bound those waits independently from the heartbeat so terminal
// output and user input cannot sit queued for the two-second heartbeat period.
const NATIVE_OUTBOUND_POLL_INTERVAL: Duration = Duration::from_millis(50);
// Loopback connects normally settle immediately. Keep each OS connect attempt
// below the lifecycle join budget so cancellation is observed before teardown
// is forced to retain worker residue.
const PORT_FORWARD_CONNECT_TIMEOUT: Duration = Duration::from_millis(100);
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
pub(crate) const AI_STARTUP_REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const GIT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const REMOTE_ACCESS_LOG_LIMIT: usize = 100;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const MAX_OUTBOUND_MESSAGES_PER_TICK: usize = 128;
pub(crate) const MAX_PENDING_REMOTE_REQUESTS: usize = 256;
const MAX_CONCURRENT_REMOTE_HOST_WORK: usize = 8;
const MAX_PENDING_HOST_ADMISSION_ATTEMPTS: usize = 16;
const CLAUDE_COMPOSER_RECONCILIATION_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_CLAUDE_COMPOSER_RECONCILIATIONS: usize = 1024;
const CODEX_COMPOSER_RECONCILIATION_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_CODEX_COMPOSER_RECONCILIATIONS: usize = 1024;
pub(crate) const REMOTE_PORT_AUTHORITY_MAX_AGE_MS: u64 = 5_000;

type SessionBootstrapProvider = Arc<dyn Fn(&str) -> Option<RemoteSessionBootstrap> + Send + Sync>;
type TerminalInputHandler =
    Arc<dyn Fn(RemoteTerminalInput, u64) -> Result<(), String> + Send + Sync>;
type TerminalResizeHandler = Arc<dyn Fn(String, SessionDimensions) + Send + Sync>;
type FocusedSessionHandler = Arc<dyn Fn(String) + Send + Sync>;

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeLifecycleTestEvent {
    ListenerStarted,
    ListenerBindFailed,
    WebListenerBindFailed,
    ClientRegistered,
    ClientRemoved,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientRegistrationTestEvent {
    BeforeFence,
    Registered,
    Rejected,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalPortForwardLifecycleTestEvent {
    ConnectionAccepted,
    AcceptanceClosed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteMachineState {
    pub host: RemoteHostConfig,
    pub known_hosts: Vec<KnownRemoteHost>,
}

impl Default for RemoteMachineState {
    fn default() -> Self {
        Self {
            host: RemoteHostConfig::default(),
            known_hosts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteHostConfig {
    pub enabled: bool,
    pub bind_address: String,
    pub port: u16,
    pub keep_hosting_in_background: bool,
    pub server_id: String,
    pub pairing_token: String,
    pub certificate_pem: String,
    pub private_key_pem: String,
    pub certificate_fingerprint: String,
    pub paired_clients: Vec<PairedRemoteClient>,
    /// Durable, explicitly non-successful admission attempts. A process crash
    /// after Phase A may leave one of these records behind, but it can never be
    /// interpreted as a Connected/Reconnected activity event or usable auth.
    pub pending_admission_attempts: Vec<PendingRemoteAdmissionAttempt>,
    pub web: WebConfig,
}

impl Default for RemoteHostConfig {
    fn default() -> Self {
        let mut config = Self {
            enabled: false,
            bind_address: "0.0.0.0".to_string(),
            port: 43871,
            keep_hosting_in_background: false,
            server_id: generate_secret("host"),
            pairing_token: generate_pairing_token(),
            certificate_pem: String::new(),
            private_key_pem: String::new(),
            certificate_fingerprint: String::new(),
            paired_clients: Vec::new(),
            pending_admission_attempts: Vec::new(),
            web: WebConfig::default(),
        };
        let _ = transport::ensure_host_tls_material(&mut config);
        config
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct PendingRemoteAdmissionAttempt {
    pub attempt_nonce: String,
    pub source: RemoteAccessSource,
    pub client_id: String,
    pub generation: u64,
    pub attempted_at_epoch_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct PairedRemoteClient {
    pub client_id: String,
    pub label: String,
    pub auth_token: String,
    pub last_seen_epoch_ms: Option<u64>,
}

impl Default for PairedRemoteClient {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            label: String::new(),
            auth_token: String::new(),
            last_seen_epoch_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct KnownRemoteHost {
    pub label: String,
    pub address: String,
    pub port: u16,
    pub server_id: String,
    pub certificate_fingerprint: String,
    pub client_id: String,
    pub auth_token: String,
    pub last_connected_epoch_ms: Option<u64>,
}

impl Default for KnownRemoteHost {
    fn default() -> Self {
        Self {
            label: String::new(),
            address: String::new(),
            port: 43871,
            server_id: String::new(),
            certificate_fingerprint: String::new(),
            client_id: String::new(),
            auth_token: String::new(),
            last_connected_epoch_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RemoteWorkspaceSnapshot {
    pub app_state: AppState,
    pub runtime_state: RuntimeState,
    pub session_views: HashMap<String, TerminalSessionView>,
    pub port_statuses: HashMap<u16, PortStatus>,
    /// Exact, generation-fenced port evidence. `port_statuses` remains only
    /// as a compatibility projection for older clients; control and colour
    /// decisions must use this map.
    #[serde(default)]
    pub port_authorities: HashMap<u16, RemotePortAuthority>,
    pub controller_client_id: Option<String>,
    pub you_have_control: bool,
    pub server_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteWorkspaceDelta {
    pub app_state: Option<AppState>,
    pub runtime_state: Option<RuntimeState>,
    pub port_statuses: Option<HashMap<u16, PortStatus>>,
    #[serde(default)]
    pub port_authorities: Option<HashMap<u16, RemotePortAuthority>>,
    pub controller_client_id: Option<String>,
    pub you_have_control: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum RemotePortAuthorityKind {
    Managed,
    ManagedUnready,
    ProvenExternal,
    Unknown,
    ProbeError,
    Free,
    Occupied,
}

/// Wire-safe, path-free diagnostics for an authority that could not be
/// established. The concrete probe text remains host-local and never crosses
/// the remote/web boundary.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum RemotePortDiagnostic {
    ProbeError,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteListenerIdentity {
    pub pid: u32,
    pub creation_time_100ns: u64,
    /// The executable path is intentionally not sent over the remote wire.
    /// This bit records that the local host captured and canonicalized it.
    pub executable_proven: bool,
    /// Path-free identity of the canonical executable. The value is only
    /// useful when compared with the host's current registry snapshot; it is
    /// not a path and is never accepted as a standalone authority.
    #[serde(default)]
    pub executable_fingerprint: Option<u64>,
}

/// An in-process capability minted only after the host has correlated the
/// complete listener and managed-process observations. It intentionally has
/// no public constructor and is skipped during wire serialization, so a
/// deserialized or hand-built DTO cannot claim host verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VerifiedPortAuthority {
    projection_fingerprint: u64,
}

impl VerifiedPortAuthority {
    fn new(authority: &RemotePortAuthority) -> Self {
        Self {
            projection_fingerprint: remote_authority_projection_fingerprint(authority),
        }
    }

    fn matches(&self, authority: &RemotePortAuthority) -> bool {
        self.projection_fingerprint == remote_authority_projection_fingerprint(authority)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemotePortAuthority {
    pub port: u16,
    pub kind: RemotePortAuthorityKind,
    #[serde(default)]
    pub diagnostic: Option<RemotePortDiagnostic>,
    pub resource: Option<ResourceFence>,
    pub listeners: Vec<RemoteListenerIdentity>,
    /// The host session that owns this listener authority. This is explicit
    /// because a PID alone can be recycled between sessions.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Redacted root identity from the live managed-process fence. The
    /// executable is represented only by the proof bit and the authority
    /// fingerprint; paths never cross the remote wire.
    #[serde(default)]
    pub root: Option<RemoteListenerIdentity>,
    pub membership_revision: u64,
    pub observation_sequence: u64,
    pub publication_sequence: u64,
    pub observed_at_epoch_ms: u64,
    pub freshness_deadline_epoch_ms: u64,
    /// Path-free binding to the exact local registry fence. A shape-only
    /// resource/listener DTO is never enough to authorize forwarding.
    #[serde(default)]
    pub managed_fence_fingerprint: Option<u64>,
    /// Present only on a host-local projection that passed the exact live
    /// fence check. This marker is deliberately not part of the wire shape.
    #[serde(skip)]
    #[serde(default)]
    pub(crate) verified: Option<VerifiedPortAuthority>,
    pub error: Option<String>,
}

impl RemotePortAuthority {
    pub fn kind(&self) -> RemotePortAuthorityKind {
        self.kind
    }

    pub fn from_rich(status: &RichPortStatus, now_epoch_ms: u64) -> Self {
        Self::from_rich_with_source_metadata(
            status,
            now_epoch_ms,
            now_epoch_ms.saturating_add(REMOTE_PORT_AUTHORITY_MAX_AGE_MS),
        )
    }

    pub fn from_rich_with_source_metadata(
        status: &RichPortStatus,
        observed_at_epoch_ms: u64,
        freshness_deadline_epoch_ms: u64,
    ) -> Self {
        let projected_kind = match status.kind() {
            RichPortStatusKind::ManagedHealthy => RemotePortAuthorityKind::Managed,
            RichPortStatusKind::ManagedUnready => RemotePortAuthorityKind::ManagedUnready,
            RichPortStatusKind::ProvenExternal => RemotePortAuthorityKind::ProvenExternal,
            RichPortStatusKind::ProbeError => RemotePortAuthorityKind::ProbeError,
            RichPortStatusKind::Occupied => RemotePortAuthorityKind::Occupied,
            RichPortStatusKind::Stopped => RemotePortAuthorityKind::Free,
            RichPortStatusKind::Starting => RemotePortAuthorityKind::Unknown,
            RichPortStatusKind::Unknown => RemotePortAuthorityKind::Unknown,
        };
        // A positive authority carrying probe detail is internally
        // inconsistent. Fail closed before it reaches a renderer or control
        // predicate rather than allowing a blue/green claim with a fault.
        let kind = if status.error().is_some()
            && matches!(
                projected_kind,
                RemotePortAuthorityKind::Managed
                    | RemotePortAuthorityKind::ManagedUnready
                    | RemotePortAuthorityKind::ProvenExternal
            ) {
            RemotePortAuthorityKind::Unknown
        } else {
            projected_kind
        };
        let resource = matches!(
            kind,
            RemotePortAuthorityKind::Managed | RemotePortAuthorityKind::ManagedUnready
        )
        .then_some(status.resource);
        // A local Starting status can retain probe detail while its process is
        // being brought up. That detail is host-only just like an explicit
        // ProbeError; never let it become a wire error string.
        let diagnostic = (kind == RemotePortAuthorityKind::ProbeError || status.error().is_some())
            .then_some(RemotePortDiagnostic::ProbeError);
        Self {
            port: status.port,
            kind,
            diagnostic,
            resource,
            listeners: status
                .listeners()
                .iter()
                .map(|listener| RemoteListenerIdentity {
                    pid: listener.pid(),
                    creation_time_100ns: listener.creation_time_100ns(),
                    executable_proven: listener.has_executable_proof(),
                    executable_fingerprint: listener
                        .canonical_executable()
                        .map(executable_fingerprint),
                })
                .collect(),
            session_id: None,
            root: None,
            membership_revision: 0,
            observation_sequence: 0,
            publication_sequence: 0,
            observed_at_epoch_ms,
            freshness_deadline_epoch_ms,
            managed_fence_fingerprint: None,
            verified: None,
            error: None,
        }
    }

    pub(crate) fn with_snapshot_metadata(
        mut self,
        publication_sequence: u64,
        membership_revision: u64,
        observation_sequence: u64,
    ) -> Self {
        self.publication_sequence = publication_sequence;
        self.membership_revision = membership_revision;
        self.observation_sequence = observation_sequence;
        self
    }

    pub(crate) fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub(crate) fn with_managed_capability(
        mut self,
        capability: &ManagedResourceCapability,
    ) -> Self {
        let managed = capability.snapshot();
        self.managed_fence_fingerprint = Some(managed.authority_fingerprint());
        self.root = Some(RemoteListenerIdentity {
            pid: managed.root().id().pid(),
            creation_time_100ns: managed.root().id().creation_time_100ns(),
            executable_proven: true,
            executable_fingerprint: Some(executable_fingerprint(
                managed.root().canonical_executable(),
            )),
        });
        for listener in &mut self.listeners {
            listener.executable_fingerprint = managed
                .member_identities()
                .iter()
                .find(|member| {
                    member.id().pid() == listener.pid
                        && member.id().creation_time_100ns() == listener.creation_time_100ns
                })
                .map(|member| executable_fingerprint(member.canonical_executable()));
        }
        self
    }

    pub fn is_fresh_at(&self, now_epoch_ms: u64) -> bool {
        self.publication_sequence > 0
            && self.observed_at_epoch_ms <= now_epoch_ms
            && now_epoch_ms.saturating_sub(self.observed_at_epoch_ms)
                <= REMOTE_PORT_AUTHORITY_MAX_AGE_MS
            && self.freshness_deadline_epoch_ms >= self.observed_at_epoch_ms
            && now_epoch_ms <= self.freshness_deadline_epoch_ms
    }

    pub(crate) fn is_host_verified(&self) -> bool {
        self.verified
            .as_ref()
            .is_some_and(|proof| proof.matches(self))
    }

    /// Check the path-free fields that must be present before a web client
    /// may describe a managed authority as exact. Session/root matching is
    /// checked separately when a local runtime session is available.
    fn has_complete_wire_authority(&self) -> bool {
        if !matches!(
            self.kind,
            RemotePortAuthorityKind::Managed | RemotePortAuthorityKind::ManagedUnready
        ) || self.resource.is_none()
            || self.diagnostic.is_some()
            || self
                .resource
                .is_some_and(|resource| resource.runtime_generation == 0)
            || self.membership_revision == 0
            || self.observation_sequence == 0
            || self.managed_fence_fingerprint.is_none()
            || self.managed_fence_fingerprint == Some(0)
            || self.error.is_some()
            || self
                .session_id
                .as_deref()
                .is_none_or(|session_id| session_id.trim().is_empty())
        {
            return false;
        }
        let Some(root) = self.root.as_ref() else {
            return false;
        };
        if root.pid == 0 || root.creation_time_100ns == 0 || !root.executable_proven {
            return false;
        }
        let mut listener_ids = HashSet::new();
        !self.listeners.is_empty()
            && self.listeners.iter().all(|listener| {
                listener.pid != 0
                    && listener.creation_time_100ns != 0
                    && listener.executable_proven
                    && listener
                        .executable_fingerprint
                        .is_some_and(|fingerprint| fingerprint != 0)
                    && listener_ids.insert((listener.pid, listener.creation_time_100ns))
            })
    }

    /// Prove that this authority is the exact current host projection for a
    /// running session and one live managed registry membership snapshot.
    /// Every identity-bearing comparison is made against the caller-owned
    /// observation timestamp and deadline; no method-local clock read may
    /// widen the proof window.
    pub(crate) fn has_exact_managed_fence_for(
        &self,
        requested_port: u16,
        session: &SessionRuntimeState,
        live: &ManagedResourceCapability,
        now_epoch_ms: u64,
        observation_time: Instant,
        deadline: Instant,
    ) -> bool {
        if observation_time > deadline
            || !self.has_complete_wire_authority()
            || !self.is_fresh_at(now_epoch_ms)
            || self.port != requested_port
            || self.session_id.as_deref() != Some(session.session_id.as_str())
            || session.status != SessionStatus::Running
            || session.reap_incomplete
            || session
                .server_launch
                .as_ref()
                .and_then(|launch| launch.port)
                != Some(requested_port)
            || live.snapshot().state() != crate::process::registry::ManagedProcessState::Running
            || !live.snapshot().is_fresh_at(observation_time)
            || self.membership_revision != live.snapshot().membership_revision()
            || self.observation_sequence != live.snapshot().observation_sequence()
            || self.managed_fence_fingerprint != Some(live.snapshot().authority_fingerprint())
            || self.resource != Some(live.snapshot().resource())
        {
            return false;
        }
        let Some(session_pid) = session.pid else {
            return false;
        };
        let Some(root) = self.root.as_ref() else {
            return false;
        };
        let live_root = live.snapshot().root();
        if session_pid != live_root.id().pid()
            || root.pid != live_root.id().pid()
            || root.creation_time_100ns != live_root.id().creation_time_100ns()
            || root.executable_fingerprint
                != Some(executable_fingerprint(live_root.canonical_executable()))
            || self.listeners.is_empty()
        {
            return false;
        }
        let mut listener_ids = HashSet::new();
        self.listeners.iter().all(|listener| {
            let Some(listener_fingerprint) = listener.executable_fingerprint else {
                return false;
            };
            listener_ids.insert((listener.pid, listener.creation_time_100ns))
                && live.snapshot().member_identities().iter().any(|member| {
                    member.id().pid() == listener.pid
                        && member.id().creation_time_100ns() == listener.creation_time_100ns
                        && executable_fingerprint(member.canonical_executable())
                            == listener_fingerprint
                })
        })
    }
}

fn remote_authority_projection_fingerprint(authority: &RemotePortAuthority) -> u64 {
    let mut hasher = DefaultHasher::new();
    authority.port.hash(&mut hasher);
    let kind = match authority.kind {
        RemotePortAuthorityKind::Managed => 0u8,
        RemotePortAuthorityKind::ManagedUnready => 1,
        RemotePortAuthorityKind::ProvenExternal => 2,
        RemotePortAuthorityKind::Unknown => 3,
        RemotePortAuthorityKind::ProbeError => 4,
        RemotePortAuthorityKind::Free => 5,
        RemotePortAuthorityKind::Occupied => 6,
    };
    kind.hash(&mut hasher);
    authority
        .resource
        .map(|resource| (resource.resource_id, resource.runtime_generation))
        .hash(&mut hasher);
    authority.listeners.len().hash(&mut hasher);
    for listener in &authority.listeners {
        (
            listener.pid,
            listener.creation_time_100ns,
            listener.executable_proven,
            listener.executable_fingerprint,
        )
            .hash(&mut hasher);
    }
    authority.session_id.hash(&mut hasher);
    authority
        .root
        .as_ref()
        .map(|root| {
            (
                root.pid,
                root.creation_time_100ns,
                root.executable_proven,
                root.executable_fingerprint,
            )
        })
        .hash(&mut hasher);
    authority.membership_revision.hash(&mut hasher);
    authority.observation_sequence.hash(&mut hasher);
    authority.publication_sequence.hash(&mut hasher);
    authority.observed_at_epoch_ms.hash(&mut hasher);
    authority.freshness_deadline_epoch_ms.hash(&mut hasher);
    authority.managed_fence_fingerprint.hash(&mut hasher);
    authority.diagnostic.hash(&mut hasher);
    authority.error.hash(&mut hasher);
    hasher.finish()
}

fn executable_fingerprint(path: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.as_os_str().hash(&mut hasher);
    hasher.finish()
}

/// Convert only exact, current host evidence into host-local authorities. A
/// complete-looking map supplied by a client or an older publication remains
/// unverified; the marker is minted here only after the canonical predicate
/// correlates the session, requested port, resource generation, root, every
/// listener executable identity, and the live registry fence.
pub(crate) fn host_verified_port_authorities_at(
    authorities: &HashMap<u16, RemotePortAuthority>,
    runtime: &RuntimeState,
    managed_snapshots: &HashMap<u16, Arc<ManagedResourceCapability>>,
    now_epoch_ms: u64,
    observation_time: Instant,
    deadline: Instant,
) -> HashMap<u16, RemotePortAuthority> {
    authorities
        .iter()
        .map(|(port, authority)| {
            let mut candidate = authority.clone();
            candidate.verified = None;
            let verified = matches!(
                candidate.kind,
                RemotePortAuthorityKind::Managed | RemotePortAuthorityKind::ManagedUnready
            )
            .then(|| {
                let session = runtime.sessions.values().find(|session| {
                    session.session_id == candidate.session_id.as_deref().unwrap_or_default()
                        && session
                            .server_launch
                            .as_ref()
                            .and_then(|launch| launch.port)
                            == Some(*port)
                })?;
                let live = managed_snapshots.get(port)?;
                candidate
                    .has_exact_managed_fence_for(
                        *port,
                        session,
                        live,
                        now_epoch_ms,
                        observation_time,
                        deadline,
                    )
                    .then(|| {
                        candidate.verified = Some(VerifiedPortAuthority::new(&candidate));
                        candidate.clone()
                    })
            })
            .flatten();
            (*port, verified.unwrap_or(candidate.clone()))
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSessionBootstrap {
    pub session_id: String,
    pub runtime: SessionRuntimeState,
    pub screen: TerminalScreenSnapshot,
    pub replay_bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RemoteSessionStreamEvent {
    Bootstrap {
        bootstrap: RemoteSessionBootstrap,
    },
    Output {
        session_id: String,
        chunk_seq: u64,
        emitted_at_epoch_ms: u64,
        bytes: Vec<u8>,
    },
    RuntimePatch {
        session_id: String,
        runtime: SessionRuntimeState,
    },
    Closed {
        session_id: String,
        runtime: SessionRuntimeState,
    },
    Removed {
        session_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientMessage {
    Hello {
        protocol_version: u32,
        client_label: String,
        auth: ClientAuth,
    },
    PortForwardHello {
        protocol_version: u32,
        server_id: String,
        client_id: String,
        auth_token: String,
        requested_port: u16,
    },
    SetFocusedSession {
        session_id: Option<String>,
    },
    SubscribeSessions {
        session_ids: Vec<String>,
    },
    UnsubscribeSessions {
        session_ids: Vec<String>,
    },
    Action {
        action: RemoteAction,
    },
    TakeControl,
    ReleaseControl,
    Ping,
    Request {
        request_id: u64,
        action: RemoteAction,
    },
    TerminalInput {
        input: RemoteTerminalInput,
        enqueued_at_epoch_ms: u64,
    },
    ResizeSession {
        session_id: String,
        dimensions: SessionDimensions,
    },
    Disconnect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ClientAuth {
    PairToken {
        token: String,
    },
    ClientToken {
        client_id: String,
        auth_token: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    HelloOk {
        protocol_version: u32,
        server_id: String,
        certificate_fingerprint: String,
        client_id: String,
        client_token: String,
        controller_client_id: Option<String>,
        you_have_control: bool,
        snapshot: RemoteWorkspaceSnapshot,
    },
    PortForwardOk,
    HelloErr {
        message: String,
    },
    Pong,
    Snapshot {
        snapshot: RemoteWorkspaceSnapshot,
    },
    Delta {
        delta: RemoteWorkspaceDelta,
    },
    SessionStream {
        event: RemoteSessionStreamEvent,
    },
    Response {
        request_id: u64,
        result: RemoteActionResult,
    },
    Error {
        message: String,
    },
    Disconnected {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteImageAttachment {
    pub mime_type: String,
    pub file_name: Option<String>,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteWebMutationAuthority {
    pub runtime_instance_id: String,
    pub connection_id: u64,
    pub client_id: String,
    pub lease_generation: Option<u64>,
}

impl Default for RemoteImageAttachment {
    fn default() -> Self {
        Self {
            mime_type: String::new(),
            file_name: None,
            bytes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RemoteTerminalInput {
    Text {
        session_id: String,
        text: String,
    },
    Bytes {
        session_id: String,
        bytes: Vec<u8>,
    },
    Control {
        session_id: String,
        bytes: Vec<u8>,
    },
    Paste {
        session_id: String,
        text: String,
    },
    Image {
        session_id: String,
        attachment: RemoteImageAttachment,
        #[serde(default)]
        authority: Option<RemoteWebMutationAuthority>,
    },
    ComposerBatch {
        session_id: String,
        text: String,
        attachments: Vec<RemoteImageAttachment>,
        #[serde(default)]
        authority: RemoteWebMutationAuthority,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RemoteAction {
    StartServer {
        command_id: String,
        focus: bool,
        dimensions: SessionDimensions,
    },
    StopServer {
        command_id: String,
    },
    RestartServer {
        command_id: String,
        dimensions: SessionDimensions,
    },
    LaunchAi {
        project_id: String,
        tab_type: TabType,
        dimensions: SessionDimensions,
    },
    OpenAiTab {
        tab_id: String,
        dimensions: SessionDimensions,
    },
    RestartAiTab {
        tab_id: String,
        dimensions: SessionDimensions,
    },
    CloseAiTab {
        tab_id: String,
    },
    OpenSshTab {
        connection_id: String,
    },
    ConnectSsh {
        connection_id: String,
        dimensions: SessionDimensions,
    },
    RestartSsh {
        connection_id: String,
        dimensions: SessionDimensions,
    },
    DisconnectSsh {
        connection_id: String,
    },
    CloseSession {
        session_id: String,
    },
    CloseTab {
        tab_id: String,
    },
    StopAllServers,
    SaveProject {
        project: Project,
    },
    DeleteProject {
        project_id: String,
    },
    SaveFolder {
        project_id: String,
        folder: ProjectFolder,
        env_file_contents: Option<String>,
    },
    DeleteFolder {
        project_id: String,
        folder_id: String,
    },
    SaveCommand {
        project_id: String,
        folder_id: String,
        command: RunCommand,
    },
    DeleteCommand {
        project_id: String,
        folder_id: String,
        command_id: String,
    },
    SaveSsh {
        connection: SSHConnection,
    },
    DeleteSsh {
        connection_id: String,
    },
    SaveSettings {
        settings: Settings,
    },
    BrowsePath {
        directories_only: bool,
        start_path: Option<String>,
    },
    ListDirectory {
        path: String,
    },
    StatPath {
        path: String,
    },
    ReadTextFile {
        path: String,
    },
    WriteTextFile {
        path: String,
        contents: String,
    },
    ScanRoot {
        root_path: String,
    },
    ScanFolder {
        folder_path: String,
    },
    SearchSession {
        session_id: String,
        query: String,
        case_sensitive: bool,
    },
    ScrollSessionToBufferLine {
        session_id: String,
        buffer_line: usize,
    },
    ScrollSessionToOffset {
        session_id: String,
        display_offset: usize,
    },
    ScrollSession {
        session_id: String,
        delta_lines: i32,
    },
    ExportSessionText {
        session_id: String,
        export: RemoteTerminalExport,
    },
    GitListRepos,
    GitStatus {
        repo_path: String,
    },
    GitLog {
        repo_path: String,
        limit: u32,
        skip: u32,
    },
    GitDiffFile {
        repo_path: String,
        file_path: String,
        staged: bool,
    },
    GitDiffCommit {
        repo_path: String,
        hash: String,
    },
    GitBranches {
        repo_path: String,
    },
    GitStage {
        repo_path: String,
        files: Vec<String>,
    },
    GitUnstage {
        repo_path: String,
        files: Vec<String>,
    },
    GitStageAll {
        repo_path: String,
    },
    GitUnstageAll {
        repo_path: String,
    },
    GitCommit {
        repo_path: String,
        summary: String,
        body: Option<String>,
    },
    GitPush {
        repo_path: String,
    },
    GitPushSetUpstream {
        repo_path: String,
        branch: String,
    },
    GitPull {
        repo_path: String,
    },
    GitFetch {
        repo_path: String,
    },
    GitSync {
        repo_path: String,
    },
    GitSwitchBranch {
        repo_path: String,
        name: String,
    },
    GitCreateBranch {
        repo_path: String,
        name: String,
    },
    GitDeleteBranch {
        repo_path: String,
        name: String,
    },
    GitGetGithubAuthStatus,
    GitRequestDeviceCode,
    GitPollForToken {
        device_code: String,
    },
    GitLogout,
    GitGenerateCommitMessage {
        repo_path: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RemoteTerminalExport {
    Screen,
    Scrollback,
    Selection { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteActionResult {
    pub ok: bool,
    pub message: Option<String>,
    pub payload: Option<RemoteActionPayload>,
}

impl RemoteActionResult {
    pub fn ok(message: impl Into<Option<String>>, payload: Option<RemoteActionPayload>) -> Self {
        Self {
            ok: true,
            message: message.into(),
            payload,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: Some(message.into()),
            payload: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RemoteActionPayload {
    SearchMatches {
        matches: Vec<TerminalSearchMatch>,
    },
    BrowsePath {
        path: Option<String>,
    },
    DirectoryEntries {
        entries: Vec<RemoteFsEntry>,
    },
    PathStat {
        entry: Option<RemoteFsEntry>,
    },
    TextFile {
        path: String,
        contents: String,
    },
    RootScan {
        entries: Vec<RootScanEntry>,
    },
    FolderScan {
        scan: ScanResult,
    },
    AiTab {
        tab_id: String,
        project_id: String,
        tab_type: TabType,
        session_id: String,
        label: Option<String>,
        session_view: Option<TerminalSessionView>,
    },
    ExportText {
        text: String,
    },
    GitRepos {
        repos: Vec<RemoteGitRepo>,
    },
    GitStatus {
        status: GitStatusResult,
    },
    GitLogEntries {
        entries: Vec<GitLogEntry>,
    },
    GitDiff {
        diff: GitDiffResult,
    },
    GitBranches {
        branches: Vec<GitBranch>,
    },
    GitCommit {
        hash: String,
    },
    GitAuthStatus {
        has_token: bool,
        username: Option<String>,
    },
    GitDeviceCode {
        device_code: DeviceCodeResponse,
    },
    GitTokenPoll {
        completed: bool,
        username: Option<String>,
    },
    GitCommitMessage {
        message: AiCommitMessage,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteFsEntry {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size_bytes: Option<u64>,
    pub modified_epoch_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteGitRepo {
    pub label: String,
    pub path: String,
}

impl Default for RemoteFsEntry {
    fn default() -> Self {
        Self {
            path: String::new(),
            name: String::new(),
            is_dir: false,
            size_bytes: None,
            modified_epoch_ms: None,
        }
    }
}

#[derive(Clone)]
pub struct RemoteClientConnectResult {
    pub client: RemoteClientHandle,
    pub server_id: String,
    pub certificate_fingerprint: String,
    pub client_id: String,
    pub client_token: String,
    pub controller_client_id: Option<String>,
    pub you_have_control: bool,
    pub snapshot: RemoteWorkspaceSnapshot,
}

#[derive(Debug)]
pub struct PendingRemoteRequest {
    client_id: String,
    /// Opaque authority issued by the host WorkspaceService. Remote payloads
    /// carry only a display hint and can never construct or widen this value.
    git_authority: Option<GitHostBinding>,
    action: RemoteAction,
    response: Option<mpsc::Sender<RemoteActionResult>>,
}

impl PendingRemoteRequest {
    /// Consume a host-queued request at the host boundary.  The authority
    /// tuple cannot be constructed or widened by a remote action payload.
    pub(crate) fn into_host_parts(
        self,
    ) -> (
        String,
        Option<GitHostBinding>,
        RemoteAction,
        Option<mpsc::Sender<RemoteActionResult>>,
    ) {
        (
            self.client_id,
            self.git_authority,
            self.action,
            self.response,
        )
    }
}

#[derive(Clone)]
pub(crate) struct RemoteHostWorkLimiter {
    inner: Arc<RemoteHostWorkLimiterInner>,
}

struct RemoteHostWorkLimiterInner {
    active: AtomicUsize,
    limit: usize,
}

pub(crate) struct RemoteHostWorkPermit {
    inner: Arc<RemoteHostWorkLimiterInner>,
}

impl RemoteHostWorkPermit {
    pub(crate) fn run<T>(self, work: impl FnOnce() -> T) -> T {
        let result = work();
        drop(self);
        result
    }
}

impl RemoteHostWorkLimiter {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new(RemoteHostWorkLimiterInner {
                active: AtomicUsize::new(0),
                limit: limit.max(1),
            }),
        }
    }

    pub(crate) fn try_acquire(&self) -> Option<RemoteHostWorkPermit> {
        let mut active = self.inner.active.load(Ordering::Acquire);
        loop {
            if active >= self.inner.limit {
                return None;
            }
            match self.inner.active.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(RemoteHostWorkPermit {
                        inner: self.inner.clone(),
                    });
                }
                Err(current) => active = current,
            }
        }
    }
}

impl Drop for RemoteHostWorkPermit {
    fn drop(&mut self) {
        let previous = self.inner.active.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
    }
}

pub(crate) fn try_enqueue_pending_request(
    inner: &RemoteHostInner,
    request: PendingRemoteRequest,
) -> Result<(), PendingRemoteRequest> {
    let Ok(mut requests) = inner.pending_requests.lock() else {
        return Err(request);
    };
    if requests.len() >= MAX_PENDING_REMOTE_REQUESTS {
        return Err(request);
    }
    requests.push(request);
    Ok(())
}

#[derive(Debug, Clone)]
pub struct RemoteHostStatus {
    pub enabled: bool,
    pub web_enabled: bool,
    pub bind_address: String,
    pub port: u16,
    pub pairing_token: String,
    pub connected_clients: usize,
    pub connected_native_clients: usize,
    pub connected_web_clients: usize,
    pub controller_client_id: Option<String>,
    pub listening: bool,
    pub listener_error: Option<String>,
    pub web_listener_error: Option<String>,
    pub last_connection_note: Option<String>,
    pub last_connection_is_error: bool,
    pub latency: RemoteLatencyStats,
    pub connect_startup_error: Option<String>,
    pub connect_listener_bound: bool,
    pub connect_encryption_required: bool,
}

impl RemoteHostStatus {
    /// `true` when any transport (TCP host or browser web UI) is enabled,
    /// meaning the GPUI app should push state updates into `RemoteHostInner`
    /// so connected clients see live data.
    pub fn any_transport_enabled(&self) -> bool {
        self.enabled || self.web_enabled
    }
}

#[derive(Debug, Clone, Default)]
pub struct RemoteLatencyStats {
    pub input_enqueue_to_host_write_ms: Option<u64>,
    pub output_host_to_client_ms: Option<u64>,
    pub output_client_to_paint_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct RemotePortForwardState {
    pub port: u16,
    pub listener_active: bool,
    pub local_port_busy: bool,
    pub message: Option<String>,
}

// remote.json has several independent writers in one process (the host
// service persisting config and the app shell persisting client-side known
// hosts). Serialize savers in this single-owner process across the complete
// read/modify/write transaction. This keeps host config and known-host updates
// from replacing each other with a stale snapshot. Separate DevManager
// processes are intentionally outside this runtime ownership model.
static REMOTE_STATE_SAVE_LOCK: Mutex<()> = Mutex::new(());
static REMOTE_STATE_SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum HostConfigPersistenceTestPhase {
    BeforeWrite,
    AfterWrite,
}
#[cfg(test)]
type HostConfigPersistenceTestHook = Arc<
    dyn Fn(&RemoteHostConfig, HostConfigPersistenceTestPhase) -> std::io::Result<()> + Send + Sync,
>;
#[cfg(test)]
static HOST_CONFIG_PERSISTENCE_TEST_HOOK: Mutex<Option<HostConfigPersistenceTestHook>> =
    Mutex::new(None);
#[cfg(test)]
type RemoteStatePermissionVerifyTestHook = Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>;
#[cfg(test)]
static REMOTE_STATE_PERMISSION_VERIFY_TEST_HOOK: Mutex<
    Option<RemoteStatePermissionVerifyTestHook>,
> = Mutex::new(None);

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteStatePersistenceIoTestPhase {
    TempSync,
    Rename,
    ParentSync,
}

#[cfg(test)]
type RemoteStatePersistenceIoTestHook =
    Arc<dyn Fn(RemoteStatePersistenceIoTestPhase, &Path) -> std::io::Result<()> + Send + Sync>;
#[cfg(test)]
static REMOTE_STATE_PERSISTENCE_IO_TEST_HOOK: Mutex<Option<RemoteStatePersistenceIoTestHook>> =
    Mutex::new(None);

pub fn load_remote_machine_state() -> Result<RemoteMachineState, PersistenceError> {
    let _guard = REMOTE_STATE_SAVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    load_remote_machine_state_locked()
}

fn load_remote_machine_state_locked() -> Result<RemoteMachineState, PersistenceError> {
    let path = remote_state_path()?;
    if !path.exists() {
        return Ok(RemoteMachineState::default());
    }
    lock_remote_state_file_permissions(&path).map_err(|source| PersistenceError::Io {
        path: path.clone(),
        source,
    })?;
    let contents = fs::read_to_string(&path).map_err(|source| PersistenceError::Io {
        path: path.clone(),
        source,
    })?;
    serde_json::from_str(&contents).map_err(|source| PersistenceError::Parse { path, source })
}

fn write_private_remote_state_temp(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path)?;
    if let Err(error) = lock_new_remote_state_file_permissions(path) {
        drop(file);
        return Err(error);
    }
    file.write_all(contents)?;
    sync_remote_state_temp(&file, path)
}

fn sync_remote_state_temp(file: &fs::File, path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(hook) = REMOTE_STATE_PERSISTENCE_IO_TEST_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        hook(RemoteStatePersistenceIoTestPhase::TempSync, path)?;
    }
    file.sync_all()
}

fn rename_remote_state_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(hook) = REMOTE_STATE_PERSISTENCE_IO_TEST_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        hook(RemoteStatePersistenceIoTestPhase::Rename, to)?;
    }
    fs::rename(from, to)
}

fn sync_remote_state_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(hook) = REMOTE_STATE_PERSISTENCE_IO_TEST_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        hook(RemoteStatePersistenceIoTestPhase::ParentSync, path)?;
    }

    #[cfg(unix)]
    {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::File::open(parent)?.sync_all()
    }
    #[cfg(windows)]
    {
        // Windows does not expose a portable directory fsync through std. The
        // temp file is flushed before rename and the ACL is verified after it;
        // the parent-directory barrier is therefore best effort on this
        // platform rather than an unportable raw-handle dependency.
        let _ = path;
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(unix)]
fn lock_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    verify_remote_state_file_permissions(path)
}

#[cfg(windows)]
fn windows_system_tool(name: &str) -> std::io::Result<PathBuf> {
    let system_root = std::env::var_os("SystemRoot")
        .ok_or_else(|| std::io::Error::new(ErrorKind::NotFound, "SystemRoot is unavailable"))?;
    let path = PathBuf::from(system_root).join("System32").join(name);
    if !path.is_file() {
        return Err(std::io::Error::new(
            ErrorKind::NotFound,
            format!("Windows system tool is unavailable: {}", path.display()),
        ));
    }
    Ok(path)
}

#[cfg(windows)]
fn run_windows_system_tool(name: &str, args: &[std::ffi::OsString]) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let tool = windows_system_tool(name)?;
    let output = std::process::Command::new(&tool)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "{} failed: {}",
                tool.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ))
    }
}

#[cfg(windows)]
fn current_windows_process_sid() -> std::io::Result<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    static PROCESS_TOKEN_SID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(sid) = PROCESS_TOKEN_SID.get() {
        return Ok(sid.clone());
    }

    let whoami = windows_system_tool("whoami.exe")?;
    let output = std::process::Command::new(&whoami)
        .args(["/user", "/fo", "csv", "/nh"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "{} failed: {}",
                whoami.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            "whoami.exe returned non-UTF-8 output",
        )
    })?;
    let sid = stdout
        .split(|character: char| character == ',' || character.is_whitespace() || character == '"')
        .find(|field| field.starts_with("S-1-"))
        .map(str::to_string)
        .ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::InvalidData,
                "whoami.exe did not return a process token SID",
            )
        })?;
    let components = sid.split('-').collect::<Vec<_>>();
    if components.len() < 4
        || components[0] != "S"
        || components[1] != "1"
        || components[2..].iter().any(|component| {
            component.is_empty() || !component.chars().all(|ch| ch.is_ascii_digit())
        })
    {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("whoami.exe returned an invalid process token SID: {sid}"),
        ));
    }
    let _ = PROCESS_TOKEN_SID.set(sid.clone());
    Ok(sid)
}

#[cfg(windows)]
fn windows_acl_sddl(path: &Path) -> std::io::Result<String> {
    let acl_path = path.with_extension(format!(
        "acl-{}-{}",
        std::process::id(),
        REMOTE_STATE_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        run_windows_system_tool(
            "icacls.exe",
            &[
                path.as_os_str().to_os_string(),
                "/save".into(),
                acl_path.as_os_str().to_os_string(),
            ],
        )?;
        let bytes = fs::read(&acl_path)?;
        if bytes.len() % 2 != 0 {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "icacls.exe wrote a malformed ACL export",
            ));
        }
        let words = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        String::from_utf16(&words).map_err(|_| {
            std::io::Error::new(
                ErrorKind::InvalidData,
                "icacls.exe wrote an invalid UTF-16 ACL export",
            )
        })
    })();
    let _ = fs::remove_file(&acl_path);
    result
}

#[cfg(windows)]
fn windows_dacl_entries(sddl_export: &str) -> std::io::Result<Vec<(String, String, String)>> {
    let dacl_start = sddl_export.find("D:").ok_or_else(|| {
        std::io::Error::new(ErrorKind::InvalidData, "ACL export is missing a DACL")
    })?;
    let dacl = &sddl_export[dacl_start + 2..];
    let dacl = dacl.split("S:").next().unwrap_or(dacl);
    let mut entries = Vec::new();
    let mut remaining = dacl;
    while let Some(start) = remaining.find('(') {
        let after_start = &remaining[start + 1..];
        let Some(end) = after_start.find(')') else {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "ACL export contains an unterminated access rule",
            ));
        };
        let fields = after_start[..end].split(';').collect::<Vec<_>>();
        if fields.len() < 6 || fields[5].trim().is_empty() {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "ACL export contains a malformed access rule",
            ));
        }
        entries.push((
            fields[0].trim().to_string(),
            fields[2].trim().to_string(),
            fields[5].trim().to_string(),
        ));
        remaining = &after_start[end + 1..];
    }
    Ok(entries)
}

#[cfg(windows)]
fn windows_trustee_sid(trustee: &str) -> Option<&str> {
    match trustee {
        "WD" => Some("S-1-1-0"),
        "AU" => Some("S-1-5-11"),
        "BU" => Some("S-1-5-32-545"),
        "BA" => Some("S-1-5-32-544"),
        "SY" => Some("S-1-5-18"),
        trustee if trustee.starts_with("S-1-") => Some(trustee),
        _ => None,
    }
}

#[cfg(windows)]
fn windows_trustee_matches_sid(trustee: &str, sid: &str) -> bool {
    windows_trustee_sid(trustee).is_some_and(|trustee_sid| trustee_sid.eq_ignore_ascii_case(sid))
        || (trustee == "LA" && sid.rsplit('-').next() == Some("500"))
}

#[cfg(windows)]
fn lock_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
    if verify_remote_state_file_permissions(path).is_ok() {
        return Ok(());
    }
    let current_sid = current_windows_process_sid()?;
    run_windows_system_tool(
        "icacls.exe",
        &[path.as_os_str().to_os_string(), "/inheritance:r".into()],
    )?;

    let initial_acl = windows_acl_sddl(path)?;
    for (_, _, trustee) in windows_dacl_entries(&initial_acl)? {
        if windows_trustee_matches_sid(&trustee, &current_sid) {
            continue;
        }
        let trustee_sid = windows_trustee_sid(&trustee).ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::PermissionDenied,
                format!("cannot safely identify ACL trustee {trustee}"),
            )
        })?;
        for removal in ["/remove:g", "/remove:d"] {
            run_windows_system_tool(
                "icacls.exe",
                &[
                    path.as_os_str().to_os_string(),
                    removal.into(),
                    format!("*{trustee_sid}").into(),
                ],
            )?;
        }
    }

    // A legacy deny for the current user must not survive the upgrade.
    run_windows_system_tool(
        "icacls.exe",
        &[
            path.as_os_str().to_os_string(),
            "/remove:d".into(),
            format!("*{current_sid}").into(),
        ],
    )?;
    run_windows_system_tool(
        "icacls.exe",
        &[
            path.as_os_str().to_os_string(),
            "/grant:r".into(),
            format!("*{current_sid}:(F)").into(),
        ],
    )?;
    verify_remote_state_file_permissions(path)
}

#[cfg(unix)]
pub(crate) fn lock_new_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
    lock_remote_state_file_permissions(path)
}

#[cfg(windows)]
pub(crate) fn lock_new_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
    lock_remote_state_file_permissions(path)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn lock_new_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
    lock_remote_state_file_permissions(path)
}

#[cfg(not(any(unix, windows)))]
fn lock_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        format!(
            "secure remote state permissions are unsupported for {}",
            path.display()
        ),
    ))
}

#[cfg(unix)]
pub(crate) fn verify_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(path)?.permissions().mode() & 0o777;
    if mode == 0o600 {
        Ok(())
    } else {
        Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            format!("remote state permissions are {mode:o}, expected 600"),
        ))
    }
}

#[cfg(windows)]
pub(crate) fn verify_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
    let current_sid = current_windows_process_sid()?;
    let entries = windows_dacl_entries(&windows_acl_sddl(path)?)?;
    if entries.len() == 1
        && entries[0].0 == "A"
        && entries[0].1 == "FA"
        && windows_trustee_matches_sid(&entries[0].2, &current_sid)
    {
        Ok(())
    } else {
        Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            format!("remote state ACL is not current-user only: {entries:?}"),
        ))
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn verify_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        format!(
            "secure remote state permissions are unsupported for {}",
            path.display()
        ),
    ))
}

fn verify_saved_remote_state_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(hook) = REMOTE_STATE_PERMISSION_VERIFY_TEST_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return hook(path);
    }
    verify_remote_state_file_permissions(path)
}

fn restore_remote_state_bytes(path: &Path, previous_bytes: Option<&[u8]>) -> std::io::Result<()> {
    let Some(previous_bytes) = previous_bytes else {
        let result = match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        result?;
        return sync_remote_state_parent(path);
    };
    let restore_path = path.with_extension(format!(
        "json.restore-{}-{}",
        std::process::id(),
        REMOTE_STATE_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(error) = write_private_remote_state_temp(&restore_path, previous_bytes) {
        let _ = fs::remove_file(&restore_path);
        return Err(error);
    }
    if let Err(error) = rename_remote_state_file(&restore_path, path) {
        let _ = fs::remove_file(&restore_path);
        return Err(error);
    }
    sync_remote_state_parent(path)?;
    verify_remote_state_file_permissions(path)
}

pub fn save_remote_machine_state(state: &RemoteMachineState) -> Result<(), PersistenceError> {
    let _guard = REMOTE_STATE_SAVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    save_remote_machine_state_locked(state)
}

/// Publish bounded native trust custody with the same private-file transaction
/// as remote state. The caller owns its store lock for the entire operation.
pub(crate) fn atomic_write_remote_state_bytes(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    const MAX_TRUST_BYTES: usize = 256 * 1024;
    if contents.len() > MAX_TRUST_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "trust file exceeds bound",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};
        options
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    let previous_bytes = match options.open(path) {
        Ok(file) => {
            let metadata = file.metadata()?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err(std::io::Error::new(
                        ErrorKind::InvalidData,
                        "trust file is a reparse point",
                    ));
                }
            }
            if !metadata.is_file() || metadata.len() > MAX_TRUST_BYTES as u64 {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "invalid trust file",
                ));
            }
            let mut bytes = Vec::new();
            std::io::Read::take(file, MAX_TRUST_BYTES as u64 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > MAX_TRUST_BYTES {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "trust file exceeds bound",
                ));
            }
            Some(bytes)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let temp_path = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        REMOTE_STATE_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(error) = write_private_remote_state_temp(&temp_path, contents) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = rename_remote_state_file(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) =
        sync_remote_state_parent(path).and_then(|()| verify_saved_remote_state_permissions(path))
    {
        let _ = restore_remote_state_bytes(path, previous_bytes.as_deref());
        return Err(error);
    }
    Ok(())
}

fn save_remote_machine_state_locked(state: &RemoteMachineState) -> Result<(), PersistenceError> {
    let path = remote_state_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| PersistenceError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let json = serde_json::to_string_pretty(state).map_err(|source| PersistenceError::Parse {
        path: path.clone(),
        source,
    })?;
    let previous_bytes = match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(source) => {
            return Err(PersistenceError::Io {
                path: path.clone(),
                source,
            });
        }
    };
    let temp_path = path.with_extension(format!(
        "json.tmp-{}-{}",
        std::process::id(),
        REMOTE_STATE_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(source) = write_private_remote_state_temp(&temp_path, json.as_bytes()) {
        let _ = fs::remove_file(&temp_path);
        return Err(PersistenceError::Io {
            path: temp_path,
            source,
        });
    }
    if let Err(source) = rename_remote_state_file(&temp_path, &path) {
        let _ = fs::remove_file(&temp_path);
        return Err(PersistenceError::Io { path, source });
    }
    if let Err(source) = sync_remote_state_parent(&path) {
        let restore_result = restore_remote_state_bytes(&path, previous_bytes.as_deref());
        return Err(PersistenceError::Io {
            path: path.clone(),
            source: match restore_result {
                Ok(()) => source,
                Err(restore_error) => std::io::Error::new(
                    source.kind(),
                    format!(
                        "remote state parent sync failed: {source}; restoring previous remote state failed: {restore_error}"
                    ),
                ),
            },
        });
    }
    if let Err(source) = verify_saved_remote_state_permissions(&path) {
        let restore_result = restore_remote_state_bytes(&path, previous_bytes.as_deref());
        return Err(PersistenceError::Io {
            path: path.clone(),
            source: match restore_result {
                Ok(()) => source,
                Err(restore_error) => std::io::Error::new(
                    source.kind(),
                    format!(
                        "post-rename permission verification failed: {source}; restoring previous remote state failed: {restore_error}"
                    ),
                ),
            },
        });
    }
    Ok(())
}

/// Patch only web-listener settings while holding the shared persistence lock.
/// Setup must not replace paired clients or unrelated remote settings from a
/// stale UI/controller snapshot.
pub(crate) fn update_web_listener_config(
    mutate: impl FnOnce(&mut web::WebConfig),
) -> Result<RemoteHostConfig, PersistenceError> {
    let _guard = REMOTE_STATE_SAVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut state = load_remote_machine_state_locked()?;
    mutate(&mut state.host.web);
    save_remote_machine_state_locked(&state)?;
    Ok(state.host)
}

pub(crate) fn persist_host_config_snapshot(
    config: &RemoteHostConfig,
) -> Result<(), PersistenceError> {
    #[cfg(test)]
    let test_hook = HOST_CONFIG_PERSISTENCE_TEST_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    #[cfg(test)]
    if let Some(hook) = test_hook.as_ref() {
        hook(config, HostConfigPersistenceTestPhase::BeforeWrite).map_err(|source| {
            PersistenceError::Io {
                path: remote_state_path().unwrap_or_else(|_| PathBuf::from(REMOTE_FILE_NAME)),
                source,
            }
        })?;
    }
    let _guard = REMOTE_STATE_SAVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut state = load_remote_machine_state_locked()?;
    state.host = config.clone();
    save_remote_machine_state_locked(&state)?;
    #[cfg(test)]
    if let Some(hook) = test_hook.as_ref() {
        let _ = hook(config, HostConfigPersistenceTestPhase::AfterWrite);
    }
    Ok(())
}

/// Restores rejected admission candidates only when the durable host section
/// is still one of those exact candidates. This compare-and-swap prevents a
/// late compensation from overwriting a newer same-client transaction.
fn restore_host_config_snapshot_if_any_current(
    expected: &[&RemoteHostConfig],
    restore: &RemoteHostConfig,
) -> Result<bool, PersistenceError> {
    #[cfg(test)]
    let test_hook = HOST_CONFIG_PERSISTENCE_TEST_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    #[cfg(test)]
    if let Some(hook) = test_hook.as_ref() {
        hook(restore, HostConfigPersistenceTestPhase::BeforeWrite).map_err(|source| {
            PersistenceError::Io {
                path: remote_state_path().unwrap_or_else(|_| PathBuf::from(REMOTE_FILE_NAME)),
                source,
            }
        })?;
    }
    let _guard = REMOTE_STATE_SAVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut state = load_remote_machine_state_locked()?;
    if !expected.iter().any(|candidate| state.host == **candidate) {
        return Ok(false);
    }
    state.host = restore.clone();
    save_remote_machine_state_locked(&state)?;
    #[cfg(test)]
    if let Some(hook) = test_hook.as_ref() {
        let _ = hook(restore, HostConfigPersistenceTestPhase::AfterWrite);
    }
    Ok(true)
}

pub(crate) struct StagedHostConfigMutation<T> {
    pub(crate) base_revision: u64,
    pub(crate) base: RemoteHostConfig,
    pub(crate) candidate: RemoteHostConfig,
    pub(crate) result: T,
}

pub(crate) fn stage_host_config_mutation<T>(
    inner: &Arc<RemoteHostInner>,
    mutate: impl FnOnce(&mut RemoteHostConfig) -> T,
) -> Result<StagedHostConfigMutation<T>, String> {
    let base_revision = inner.config_revision.load(Ordering::Acquire);
    let base = inner
        .config
        .read()
        .map_err(|_| "host config unavailable".to_string())?
        .clone();
    let mut candidate = base.clone();
    let result = mutate(&mut candidate);
    Ok(StagedHostConfigMutation {
        base_revision,
        base,
        candidate,
        result,
    })
}

fn commit_staged_host_config_mutation<T>(
    inner: &Arc<RemoteHostInner>,
    staged: &StagedHostConfigMutation<T>,
) -> Result<(), String> {
    if inner.config_revision.load(Ordering::Acquire) != staged.base_revision {
        return Err("host config changed during its serialized transaction".to_string());
    }
    let mut config = inner
        .config
        .write()
        .map_err(|_| "host config unavailable".to_string())?;
    if *config != staged.base {
        return Err("host config changed during its serialized transaction".to_string());
    }
    *config = staged.candidate.clone();
    drop(config);
    bump_host_config_revision(inner);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostConfigAdmissionError {
    Persistence(String),
    DurabilityUncertain { attempt_id: u64, detail: String },
}

impl std::fmt::Display for HostConfigAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Persistence(detail) => formatter.write_str(detail),
            Self::DurabilityUncertain { attempt_id, detail } => write!(
                formatter,
                "Remote host configuration durability is uncertain for attempt {attempt_id}: {detail}"
            ),
        }
    }
}

impl std::error::Error for HostConfigAdmissionError {}

pub(crate) fn compensate_rejected_host_config_admission<T>(
    staged: &StagedHostConfigMutation<T>,
    attempt_id: u64,
) -> Result<(), HostConfigAdmissionError> {
    compensate_rejected_host_config_candidates(&[&staged.candidate], &staged.base, attempt_id)
}

pub(crate) fn compensate_rejected_host_config_candidates(
    expected: &[&RemoteHostConfig],
    restore: &RemoteHostConfig,
    attempt_id: u64,
) -> Result<(), HostConfigAdmissionError> {
    match restore_host_config_snapshot_if_any_current(expected, restore) {
        Ok(true) => Ok(()),
        Ok(false) => Err(HostConfigAdmissionError::DurabilityUncertain {
            attempt_id,
            detail: "the durable host config no longer matched the rejected candidate; no newer state was overwritten"
                .to_string(),
        }),
        Err(error) => Err(HostConfigAdmissionError::DurabilityUncertain {
            attempt_id,
            detail: format!("conditional compensation failed: {error}"),
        }),
    }
}

pub fn save_remote_known_hosts(known_hosts: &[KnownRemoteHost]) -> Result<(), PersistenceError> {
    let _guard = REMOTE_STATE_SAVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut state = load_remote_machine_state_locked()?;
    state.known_hosts = known_hosts.to_vec();
    save_remote_machine_state_locked(&state)
}

pub(crate) fn mutate_host_config_if<T>(
    inner: &Arc<RemoteHostInner>,
    condition: impl FnOnce(&RemoteHostConfig) -> bool,
    mutate: impl FnOnce(&mut RemoteHostConfig) -> T,
) -> Result<Option<T>, HostConfigAdmissionError> {
    let _update_guard = inner.host_config_tx.lock().map_err(|_| {
        HostConfigAdmissionError::Persistence("host config update unavailable".to_string())
    })?;
    let matches = inner
        .config
        .read()
        .map_err(|_| HostConfigAdmissionError::Persistence("host config unavailable".to_string()))
        .map(|config| condition(&config))?;
    if !matches {
        return Ok(None);
    }
    let attempt_id = inner
        .next_host_config_attempt_id
        .fetch_add(1, Ordering::Relaxed);
    let staged =
        stage_host_config_mutation(inner, mutate).map_err(HostConfigAdmissionError::Persistence)?;
    persist_host_config_snapshot(&staged.candidate)
        .map_err(|error| HostConfigAdmissionError::Persistence(error.to_string()))?;
    if let Err(error) = commit_staged_host_config_mutation(inner, &staged) {
        let compensate = compensate_rejected_host_config_admission(&staged, attempt_id);
        // Wake leases after rollback so they re-check committed truth.
        bump_host_config_revision(inner);
        compensate?;
        return Err(HostConfigAdmissionError::Persistence(error));
    }
    Ok(Some(staged.result))
}

pub(crate) fn mutate_host_config<T>(
    inner: &Arc<RemoteHostInner>,
    mutate: impl FnOnce(&mut RemoteHostConfig) -> T,
) -> Result<T, HostConfigAdmissionError> {
    let _update_guard = inner.host_config_tx.lock().map_err(|_| {
        HostConfigAdmissionError::Persistence("host config update unavailable".to_string())
    })?;
    let attempt_id = inner
        .next_host_config_attempt_id
        .fetch_add(1, Ordering::Relaxed);
    let staged =
        stage_host_config_mutation(inner, mutate).map_err(HostConfigAdmissionError::Persistence)?;
    persist_host_config_snapshot(&staged.candidate)
        .map_err(|error| HostConfigAdmissionError::Persistence(error.to_string()))?;
    if let Err(error) = commit_staged_host_config_mutation(inner, &staged) {
        let compensate = compensate_rejected_host_config_admission(&staged, attempt_id);
        bump_host_config_revision(inner);
        compensate?;
        return Err(HostConfigAdmissionError::Persistence(error));
    }
    Ok(staged.result)
}

pub(crate) fn append_pending_admission_attempt(
    config: &mut RemoteHostConfig,
    attempt: PendingRemoteAdmissionAttempt,
) {
    config
        .pending_admission_attempts
        .retain(|existing| existing.attempt_nonce != attempt.attempt_nonce);
    if config.pending_admission_attempts.len() >= MAX_PENDING_HOST_ADMISSION_ATTEMPTS {
        let overflow = config
            .pending_admission_attempts
            .len()
            .saturating_add(1)
            .saturating_sub(MAX_PENDING_HOST_ADMISSION_ATTEMPTS);
        config.pending_admission_attempts.drain(..overflow);
    }
    config.pending_admission_attempts.push(attempt);
}

pub(crate) fn remove_pending_admission_attempt(
    config: &mut RemoteHostConfig,
    attempt_nonce: &str,
) -> bool {
    let before = config.pending_admission_attempts.len();
    config
        .pending_admission_attempts
        .retain(|attempt| attempt.attempt_nonce != attempt_nonce);
    config.pending_admission_attempts.len() != before
}

fn append_native_connection_activity(
    config: &mut RemoteHostConfig,
    client_id: String,
    label: String,
    ip_address: Option<String>,
    occurred_at_epoch_ms: u64,
) {
    let had_previous_connect = config.web.activity_log.iter().any(|event| {
        event.source == RemoteAccessSource::NativeApp
            && event.client_id == client_id
            && matches!(
                event.event_kind,
                RemoteAccessActivityKind::Connected | RemoteAccessActivityKind::Reconnected
            )
    });
    append_remote_access_activity_event(
        config,
        RemoteAccessActivityEvent {
            client_id,
            source: RemoteAccessSource::NativeApp,
            event_kind: if had_previous_connect {
                RemoteAccessActivityKind::Reconnected
            } else {
                RemoteAccessActivityKind::Connected
            },
            label,
            ip_address,
            event_at_epoch_ms: Some(occurred_at_epoch_ms),
            browser_family: None,
            browser_version: None,
            os_family: None,
            device_class: Some("desktop".to_string()),
        },
    );
}

pub(crate) fn append_remote_access_activity_event(
    config: &mut RemoteHostConfig,
    event: RemoteAccessActivityEvent,
) {
    config.web.activity_log.push(event);
    if config.web.activity_log.len() > REMOTE_ACCESS_LOG_LIMIT {
        let overflow = config
            .web
            .activity_log
            .len()
            .saturating_sub(REMOTE_ACCESS_LOG_LIMIT);
        config.web.activity_log.drain(0..overflow);
    }
}

pub fn remote_state_path() -> Result<PathBuf, PersistenceError> {
    Ok(persistence::app_config_dir()?.join(REMOTE_FILE_NAME))
}

pub fn generate_pairing_token() -> String {
    web::auth::generate_web_pairing_token()
}

pub fn upsert_known_host(
    state: &mut RemoteMachineState,
    label: String,
    address: String,
    port: u16,
    server_id: String,
    certificate_fingerprint: String,
    client_id: String,
    auth_token: String,
) {
    if let Some(existing) = state
        .known_hosts
        .iter_mut()
        .find(|host| host.server_id == server_id)
    {
        existing.label = label;
        existing.address = address;
        existing.port = port;
        existing.certificate_fingerprint = certificate_fingerprint;
        existing.client_id = client_id;
        existing.auth_token = auth_token;
        existing.last_connected_epoch_ms = Some(now_epoch_ms());
        return;
    }

    state.known_hosts.push(KnownRemoteHost {
        label,
        address,
        port,
        server_id,
        certificate_fingerprint,
        client_id,
        auth_token,
        last_connected_epoch_ms: Some(now_epoch_ms()),
    });
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn generate_secret(prefix: &str) -> String {
    let mut bytes = [0_u8; 24];
    getrandom::fill(&mut bytes).unwrap_or_else(|error| {
        panic!("Cannot generate native remote credential from the operating system RNG: {error}")
    });
    let random_hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{prefix}-{random_hex}")
}

fn session_ids_for_open_tabs(state: &AppState) -> HashSet<String> {
    state
        .open_tabs
        .iter()
        .filter_map(|tab| match tab.tab_type {
            TabType::Server => tab.command_id.clone(),
            TabType::Claude | TabType::Codex | TabType::Ssh => tab
                .pty_session_id
                .clone()
                .or_else(|| tab.command_id.clone()),
        })
        .collect()
}

pub struct RemoteHostService {
    inner: Arc<RemoteHostInner>,
    _lifetime_owner: Option<RemoteHostServiceOwner>,
}

/// Non-owning application callback handle. Long-lived callback payloads must
/// capture this rather than a `RemoteHostService` clone so a stalled callback
/// cannot keep a stopped host runtime alive. Upgrade only for the immediate
/// synchronous operation and discard the borrowed service before waiting.
#[derive(Clone)]
pub struct RemoteHostWeakHandle {
    inner: Weak<RemoteHostInner>,
}

impl RemoteHostWeakHandle {
    pub fn upgrade(&self) -> Option<RemoteHostService> {
        self.inner.upgrade().and_then(|inner| {
            if inner.stop_flag.load(Ordering::Acquire) {
                None
            } else {
                Some(RemoteHostService::borrowed(inner))
            }
        })
    }
}

struct RemoteWorkerAdmissionPool {
    capacity: usize,
    in_use: AtomicUsize,
}

impl RemoteWorkerAdmissionPool {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            in_use: AtomicUsize::new(0),
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<RemoteWorkerPermit> {
        let mut current = self.in_use.load(Ordering::Acquire);
        loop {
            if current >= self.capacity {
                return None;
            }
            match self.in_use.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(RemoteWorkerPermit {
                        pool: Arc::clone(self),
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }

    #[cfg(test)]
    fn in_use(&self) -> usize {
        self.in_use.load(Ordering::Acquire)
    }
}

struct RemoteWorkerPermit {
    pool: Arc<RemoteWorkerAdmissionPool>,
}

impl RemoteWorkerPermit {
    /// Release only after the owned OS thread has been joined. A permit that
    /// is dropped without this call intentionally fails closed and leaves the
    /// admission slot consumed rather than allowing a detached worker to
    /// create unbounded lifecycle residue.
    fn release(self) {
        self.pool.in_use.fetch_sub(1, Ordering::AcqRel);
    }
}

struct RemoteWorkerJoinHandle {
    handle: Option<thread::JoinHandle<()>>,
    permit: Option<RemoteWorkerPermit>,
}

impl RemoteWorkerJoinHandle {
    fn is_finished(&self) -> bool {
        self.handle
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished)
    }

    fn thread(&self) -> &thread::Thread {
        self.handle.as_ref().expect("remote worker handle").thread()
    }

    fn join(mut self) -> thread::Result<()> {
        let result = self.handle.take().expect("remote worker handle").join();
        self.permit
            .take()
            .expect("remote worker admission permit")
            .release();
        result
    }
}

#[derive(Debug)]
pub(in crate::remote) enum RemoteWorkerSpawnError {
    AdmissionUnavailable {
        name: String,
    },
    Os {
        name: String,
        source: std::io::Error,
    },
}

impl std::fmt::Display for RemoteWorkerSpawnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AdmissionUnavailable { name } => {
                write!(formatter, "remote worker admission unavailable for {name}")
            }
            Self::Os { name, source } => {
                write!(formatter, "could not spawn remote worker {name}: {source}")
            }
        }
    }
}

pub(in crate::remote) struct RemoteWorker {
    name: String,
    completion_rx: mpsc::Receiver<()>,
    handle: Option<RemoteWorkerJoinHandle>,
}

struct RemoteWorkerCompletion {
    completion_tx: Option<mpsc::SyncSender<()>>,
    done: Option<Arc<AtomicBool>>,
}

impl Drop for RemoteWorkerCompletion {
    fn drop(&mut self) {
        if let Some(done) = self.done.as_ref() {
            done.store(true, Ordering::Release);
        }
        if let Some(completion_tx) = self.completion_tx.take() {
            let _ = completion_tx.try_send(());
        }
        notify_remote_worker_reaper();
    }
}

impl RemoteWorker {
    pub(in crate::remote) fn try_spawn(
        name: impl Into<String>,
        done: Option<Arc<AtomicBool>>,
        job: impl FnOnce() + Send + 'static,
    ) -> Result<Self, RemoteWorkerSpawnError> {
        Self::try_spawn_with_pool(remote_worker_admission_pool(), name, done, job)
    }

    fn try_spawn_with_pool(
        pool: Arc<RemoteWorkerAdmissionPool>,
        name: impl Into<String>,
        done: Option<Arc<AtomicBool>>,
        job: impl FnOnce() + Send + 'static,
    ) -> Result<Self, RemoteWorkerSpawnError> {
        let name = name.into();
        let Some(permit) = pool.try_acquire() else {
            return Err(RemoteWorkerSpawnError::AdmissionUnavailable { name: name.clone() });
        };
        Self::try_spawn_with_permit(permit, name, done, job)
    }

    fn try_spawn_with_permit(
        permit: RemoteWorkerPermit,
        name: impl Into<String>,
        done: Option<Arc<AtomicBool>>,
        job: impl FnOnce() + Send + 'static,
    ) -> Result<Self, RemoteWorkerSpawnError> {
        let name = name.into();
        let (completion_tx, completion_rx) = mpsc::sync_channel(1);
        let handle = thread::Builder::new().name(name.clone()).spawn(move || {
            let _completion = RemoteWorkerCompletion {
                completion_tx: Some(completion_tx),
                done,
            };
            job();
        });
        let handle = match handle {
            Ok(handle) => handle,
            Err(source) => {
                permit.release();
                return Err(RemoteWorkerSpawnError::Os { name, source });
            }
        };
        Ok(Self {
            name,
            completion_rx,
            handle: Some(RemoteWorkerJoinHandle {
                handle: Some(handle),
                permit: Some(permit),
            }),
        })
    }

    #[cfg(test)]
    fn spawn(
        name: impl Into<String>,
        done: Option<Arc<AtomicBool>>,
        job: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self::try_spawn(name, done, job).unwrap_or_else(|error| panic!("{error}"))
    }

    fn is_finished(&self) -> bool {
        self.handle
            .as_ref()
            .is_none_or(RemoteWorkerJoinHandle::is_finished)
    }

    fn join(mut self) -> thread::Result<()> {
        self.handle.take().expect("remote worker handle").join()
    }
}

fn remote_worker_admission_pool() -> Arc<RemoteWorkerAdmissionPool> {
    REMOTE_WORKER_ADMISSION_POOL
        .get_or_init(|| {
            Arc::new(RemoteWorkerAdmissionPool::new(
                REMOTE_WORKER_ADMISSION_CAPACITY,
            ))
        })
        .clone()
}

struct NativeConnectionWorker {
    generation: u64,
    done: Arc<AtomicBool>,
    cancellation: Arc<ForwardCancellation>,
    worker: RemoteWorker,
}

#[derive(Default)]
struct ForwardCancellation {
    cancelled: AtomicBool,
    endpoints: Mutex<Vec<TcpStream>>,
    #[cfg(test)]
    write_blocked_observer: Mutex<Option<mpsc::SyncSender<()>>>,
}

impl ForwardCancellation {
    fn register(&self, endpoint: &TcpStream) -> bool {
        let Ok(clone) = endpoint.try_clone() else {
            return false;
        };
        let mut endpoints = self
            .endpoints
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.cancelled.load(Ordering::Acquire) {
            let _ = clone.shutdown(Shutdown::Both);
            return false;
        }
        endpoints.push(clone);
        true
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let endpoints = self
            .endpoints
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for endpoint in endpoints.iter() {
            let _ = endpoint.shutdown(Shutdown::Both);
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn set_write_blocked_observer(&self, observer: Option<mpsc::SyncSender<()>>) {
        *self
            .write_blocked_observer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = observer;
    }

    #[cfg(test)]
    fn notify_write_blocked(&self) {
        let observer = self
            .write_blocked_observer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(observer) = observer {
            let _ = observer.try_send(());
        }
    }
}

struct DeferredRemoteWorker {
    name: String,
    generation: u64,
    handle: RemoteWorkerJoinHandle,
    owner: DeferredRemoteWorkerOwner,
    #[cfg(test)]
    reap_observer: Option<mpsc::SyncSender<RemoteWorkerReapedEvent>>,
}

enum DeferredRemoteWorkerOwner {
    Host(Weak<RemoteHostInner>),
    LocalPortForward {
        inner: Weak<LocalPortForwardManagerInner>,
        port: u16,
    },
    Unowned,
}

const REMOTE_WORKER_REAPER_QUEUE_CAPACITY: usize = 64;
const REMOTE_WORKER_REAPER_FALLBACK_CAPACITY: usize = 64;
const REMOTE_WORKER_REAPER_PENDING_CAPACITY: usize =
    REMOTE_WORKER_REAPER_QUEUE_CAPACITY + REMOTE_WORKER_REAPER_FALLBACK_CAPACITY;
// The queue and fallback registry together are the only bounded owners for a
// worker that outlives its caller. No production worker may be admitted beyond
// that total, so the fallback can retain every admitted residue even while a
// reaper channel is unavailable.
const REMOTE_WORKER_ADMISSION_CAPACITY: usize = REMOTE_WORKER_REAPER_PENDING_CAPACITY;

struct RemoteWorkerReaper {
    sender: Mutex<Option<mpsc::SyncSender<DeferredRemoteWorker>>>,
    handle: Mutex<Option<thread::JoinHandle<()>>>,
    fallback: Arc<Mutex<VecDeque<DeferredRemoteWorker>>>,
    signal: Arc<(Mutex<u64>, Condvar)>,
    lifecycle: Mutex<()>,
}

static REMOTE_WORKER_REAPER: OnceLock<RemoteWorkerReaper> = OnceLock::new();
static REMOTE_WORKER_REAPER_SIGNAL: OnceLock<Arc<(Mutex<u64>, Condvar)>> = OnceLock::new();
static REMOTE_WORKER_ADMISSION_POOL: OnceLock<Arc<RemoteWorkerAdmissionPool>> = OnceLock::new();

enum DeferredRemoteWorkerAdmission {
    Accepted,
    Full(DeferredRemoteWorker),
    Closed(DeferredRemoteWorker),
    Unavailable(DeferredRemoteWorker),
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteWorkerReapedEvent {
    generation: u64,
    name: String,
}

fn remote_worker_reaper_signal() -> &'static Arc<(Mutex<u64>, Condvar)> {
    REMOTE_WORKER_REAPER_SIGNAL.get_or_init(|| Arc::new((Mutex::new(0), Condvar::new())))
}

fn notify_remote_worker_reaper_signal(signal: &Arc<(Mutex<u64>, Condvar)>) {
    if let Ok(mut sequence) = signal.0.lock() {
        *sequence = sequence.wrapping_add(1);
        // Test-owned restart reapers share the completion signal with the
        // process-global reaper. Wake every waiter so a local reaper cannot
        // remain asleep while another owner consumes the single notification.
        signal.1.notify_all();
    }
}

fn notify_remote_worker_reaper() {
    notify_remote_worker_reaper_signal(remote_worker_reaper_signal());
}

fn drain_deferred_worker_fallback(
    fallback: &Arc<Mutex<VecDeque<DeferredRemoteWorker>>>,
    pending: &mut Vec<DeferredRemoteWorker>,
) {
    let mut fallback = fallback
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while pending.len() < REMOTE_WORKER_REAPER_PENDING_CAPACITY {
        let Some(worker) = fallback.pop_front() else {
            break;
        };
        pending.push(worker);
    }
}

/// How often the reaper re-checks workers it already holds. Only used when
/// `pending` is non-empty: a worker thread finishing does not signal the
/// reaper's condvar, so held workers must be polled or they are stranded
/// until an unrelated deferral arrives.
const REMOTE_WORKER_REAPER_POLL_INTERVAL: Duration = Duration::from_millis(25);

fn spawn_remote_worker_reaper(
    receiver: mpsc::Receiver<DeferredRemoteWorker>,
    signal: Arc<(Mutex<u64>, Condvar)>,
    fallback: Arc<Mutex<VecDeque<DeferredRemoteWorker>>>,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("remote-worker-reaper".to_string())
        .spawn(move || {
            let mut pending = Vec::with_capacity(REMOTE_WORKER_REAPER_PENDING_CAPACITY);
            let mut observed_sequence = 0_u64;
            let mut receiver_closed = false;
            loop {
                drain_deferred_worker_fallback(&fallback, &mut pending);
                while !receiver_closed && pending.len() < REMOTE_WORKER_REAPER_PENDING_CAPACITY {
                    match receiver.try_recv() {
                        Ok(worker) => pending.push(worker),
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => receiver_closed = true,
                    }
                }

                let mut index = 0;
                while index < pending.len() {
                    if pending[index].handle.is_finished() {
                        finish_deferred_remote_worker(pending.swap_remove(index));
                    } else {
                        index += 1;
                    }
                }

                if receiver_closed && pending.is_empty() {
                    let fallback_empty = fallback
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .is_empty();
                    if fallback_empty {
                        break;
                    }
                    continue;
                }

                let guard = signal
                    .0
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if *guard == observed_sequence {
                    if pending.is_empty() {
                        // Nothing in hand: the only thing that can create work
                        // is a new deferral, and that always signals. Sleep
                        // until then rather than spinning.
                        let guard = signal
                            .1
                            .wait(guard)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        observed_sequence = *guard;
                    } else {
                        // Workers already in hand finish on their own threads,
                        // and thread completion is NOT a wake source -- nothing
                        // signals this condvar when a worker returns. Waiting
                        // without a timeout here strands a worker that finished
                        // moments after the scan above, until some unrelated
                        // deferral happens to wake us. Poll instead.
                        let (guard, _timed_out) = signal
                            .1
                            .wait_timeout(guard, REMOTE_WORKER_REAPER_POLL_INTERVAL)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        observed_sequence = *guard;
                    }
                } else {
                    observed_sequence = *guard;
                }
            }
        })
}

fn remote_worker_reaper() -> &'static RemoteWorkerReaper {
    REMOTE_WORKER_REAPER.get_or_init(RemoteWorkerReaper::new)
}

impl RemoteWorkerReaper {
    fn new() -> Self {
        let reaper = Self {
            sender: Mutex::new(None),
            handle: Mutex::new(None),
            fallback: Arc::new(Mutex::new(VecDeque::new())),
            signal: remote_worker_reaper_signal().clone(),
            lifecycle: Mutex::new(()),
        };
        if let Err(error) = reaper.start() {
            eprintln!("[remote] deferred worker reaper could not start: {error}");
        }
        reaper
    }

    fn start(&self) -> std::io::Result<()> {
        let (sender, receiver) =
            mpsc::sync_channel::<DeferredRemoteWorker>(REMOTE_WORKER_REAPER_QUEUE_CAPACITY);
        let handle =
            spawn_remote_worker_reaper(receiver, self.signal.clone(), self.fallback.clone())?;
        *self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sender);
        *self
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
        Ok(())
    }

    fn send(&self, worker: DeferredRemoteWorker) -> DeferredRemoteWorkerAdmission {
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(sender) = sender else {
            return DeferredRemoteWorkerAdmission::Unavailable(worker);
        };
        match sender.try_send(worker) {
            Ok(()) => DeferredRemoteWorkerAdmission::Accepted,
            Err(mpsc::TrySendError::Full(worker)) => DeferredRemoteWorkerAdmission::Full(worker),
            Err(mpsc::TrySendError::Disconnected(worker)) => {
                DeferredRemoteWorkerAdmission::Closed(worker)
            }
        }
    }

    fn restart(&self) -> std::io::Result<()> {
        let _lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous_sender = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(previous_sender);
        notify_remote_worker_reaper_signal(&self.signal);
        let previous_handle = self
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(previous_handle) = previous_handle {
            if previous_handle.join().is_err() {
                eprintln!("[remote] deferred worker reaper exited while restarting");
            }
        }
        self.start()
    }

    fn retain_after_failure(&self, worker: DeferredRemoteWorker, detail: &str) {
        report_deferred_worker_reaper_failure(&worker, detail);
        let mut fallback = self
            .fallback
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The admission pool bounds production workers at queue + fallback.
        // A stopped or full channel may temporarily require the fallback to
        // own the whole admitted set, so never synchronously join here and do
        // not impose a smaller second threshold that could detach ownership.
        if fallback.len() >= REMOTE_WORKER_ADMISSION_CAPACITY {
            report_deferred_worker_reaper_failure(
                &worker,
                "the bounded admission registry was exhausted; residue remains in the owned fallback",
            );
        }
        fallback.push_back(worker);
        drop(fallback);
        notify_remote_worker_reaper_signal(&self.signal);
    }
}

impl Drop for RemoteWorkerReaper {
    fn drop(&mut self) {
        let sender = self
            .sender
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(sender);
        notify_remote_worker_reaper_signal(&self.signal);
        if let Some(handle) = self
            .handle
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            if handle.join().is_err() {
                eprintln!("[remote] deferred worker reaper exited while being dropped");
            }
        }
        let mut fallback = self
            .fallback
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while let Some(worker) = fallback.pop_front() {
            finish_deferred_remote_worker(worker);
        }
    }
}

fn enqueue_deferred_remote_worker(worker: DeferredRemoteWorker) {
    enqueue_deferred_remote_worker_with_reaper(remote_worker_reaper(), worker);
}

fn enqueue_after_reaper_restart(
    reaper: &RemoteWorkerReaper,
    worker: DeferredRemoteWorker,
    context: &str,
) {
    match reaper.send(worker) {
        DeferredRemoteWorkerAdmission::Accepted => {
            notify_remote_worker_reaper_signal(&reaper.signal);
        }
        DeferredRemoteWorkerAdmission::Full(worker) => reaper.retain_after_failure(
            worker,
            &format!("{context} reaper queue was full; explicit backpressure retained the residue"),
        ),
        DeferredRemoteWorkerAdmission::Closed(worker) => reaper.retain_after_failure(
            worker,
            &format!("{context} reaper channel was already closed; residue retained"),
        ),
        DeferredRemoteWorkerAdmission::Unavailable(worker) => reaper.retain_after_failure(
            worker,
            &format!("{context} reaper was unavailable; residue retained"),
        ),
    }
}

fn enqueue_deferred_remote_worker_with_reaper(
    reaper: &RemoteWorkerReaper,
    worker: DeferredRemoteWorker,
) {
    match reaper.send(worker) {
        DeferredRemoteWorkerAdmission::Accepted => {
            notify_remote_worker_reaper_signal(&reaper.signal);
        }
        DeferredRemoteWorkerAdmission::Full(worker) => {
            reaper.retain_after_failure(
                worker,
                "the bounded reaper queue was full; explicit backpressure retained the residue",
            );
        }
        DeferredRemoteWorkerAdmission::Closed(worker) => {
            report_deferred_worker_reaper_failure(&worker, "the reaper channel was closed");
            match reaper.restart() {
                Ok(()) => enqueue_after_reaper_restart(reaper, worker, "restarted"),
                Err(error) => reaper.retain_after_failure(
                    worker,
                    &format!("the reaper could not be restarted: {error}"),
                ),
            }
        }
        DeferredRemoteWorkerAdmission::Unavailable(worker) => {
            report_deferred_worker_reaper_failure(
                &worker,
                "the reaper has no running worker and startup was unavailable",
            );
            match reaper.restart() {
                Ok(()) => enqueue_after_reaper_restart(reaper, worker, "newly started"),
                Err(error) => reaper
                    .retain_after_failure(worker, &format!("the reaper could not start: {error}")),
            }
        }
    }
}

fn finish_deferred_remote_worker(worker: DeferredRemoteWorker) {
    let DeferredRemoteWorker {
        name,
        generation,
        handle,
        owner,
        #[cfg(test)]
        reap_observer,
    } = worker;
    #[cfg(not(test))]
    let _ = generation;
    if handle.join().is_err() {
        eprintln!("[remote] deferred worker {name} panicked during shutdown");
    }
    match owner {
        DeferredRemoteWorkerOwner::Host(owner) => {
            if let Some(inner) = owner.upgrade() {
                let previous = inner.worker_residue_count.fetch_sub(1, Ordering::AcqRel);
                if previous == 1 {
                    let residue_is_current = inner
                        .last_connection_note
                        .read()
                        .ok()
                        .and_then(|note| note.clone())
                        .is_some_and(|note| {
                            note.contains("Remote worker residue") && note.contains(&name)
                        });
                    if residue_is_current {
                        set_last_connection_note(
                            &inner,
                            format!("Remote worker {name} finished its deferred shutdown."),
                            false,
                        );
                    }
                }
            }
        }
        DeferredRemoteWorkerOwner::LocalPortForward { inner, port } => {
            if let Some(inner) = inner.upgrade() {
                inner.worker_residue_count.fetch_sub(1, Ordering::AcqRel);
                let residue_is_current = inner
                    .statuses
                    .read()
                    .ok()
                    .and_then(|statuses| statuses.get(&port).cloned())
                    .and_then(|state| state.message)
                    .is_some_and(|message| {
                        message.contains("worker residue") && message.contains(&name)
                    });
                if residue_is_current {
                    set_port_forward_state(
                        &inner,
                        RemotePortForwardState {
                            port,
                            listener_active: false,
                            local_port_busy: false,
                            message: Some(format!(
                                "Local forward worker {name} finished its deferred shutdown."
                            )),
                        },
                    );
                }
            }
        }
        DeferredRemoteWorkerOwner::Unowned => {}
    }
    #[cfg(test)]
    if let Some(reap_observer) = reap_observer {
        let _ = reap_observer.try_send(RemoteWorkerReapedEvent { generation, name });
    }
}

fn report_deferred_worker_reaper_failure(worker: &DeferredRemoteWorker, detail: &str) {
    match &worker.owner {
        DeferredRemoteWorkerOwner::Host(owner) => {
            if let Some(inner) = owner.upgrade() {
                set_last_connection_note(
                    &inner,
                    format!(
                        "Remote worker residue: {} retained because {detail}; DevManager still owns it until cooperative shutdown completes.",
                        worker.name
                    ),
                    true,
                );
            }
        }
        DeferredRemoteWorkerOwner::LocalPortForward { inner, port } => {
            if let Some(inner) = inner.upgrade() {
                set_port_forward_state(
                    &inner,
                    RemotePortForwardState {
                        port: *port,
                        listener_active: false,
                        local_port_busy: false,
                        message: Some(format!(
                            "Local forward worker {} retained because {detail}.",
                            worker.name
                        )),
                    },
                );
            }
        }
        DeferredRemoteWorkerOwner::Unowned => {
            eprintln!(
                "[remote] deferred worker {} retained because {detail}",
                worker.name
            );
        }
    }
}

fn defer_remote_worker(inner: &Arc<RemoteHostInner>, mut worker: RemoteWorker) {
    let Some(handle) = worker.handle.take() else {
        return;
    };
    inner.worker_residue_count.fetch_add(1, Ordering::AcqRel);
    set_last_connection_note(
        inner,
        format!(
            "Remote worker residue: {} did not stop within {} ms; DevManager still owns it until cooperative shutdown completes.",
            worker.name,
            REMOTE_WORKER_SHUTDOWN_TIMEOUT.as_millis()
        ),
        true,
    );
    #[cfg(test)]
    let reap_observer = inner
        .worker_reaped_test_hook
        .read()
        .ok()
        .and_then(|slot| slot.clone());
    enqueue_deferred_remote_worker(DeferredRemoteWorker {
        name: worker.name,
        generation: inner.native_runtime_generation.load(Ordering::Acquire),
        handle,
        owner: DeferredRemoteWorkerOwner::Host(Arc::downgrade(inner)),
        #[cfg(test)]
        reap_observer,
    });
}

pub(in crate::remote) fn settle_remote_worker(
    inner: &Arc<RemoteHostInner>,
    mut worker: RemoteWorker,
    deadline: Instant,
) {
    let Some(handle) = worker.handle.as_ref() else {
        return;
    };
    if handle.thread().id() == thread::current().id() {
        defer_remote_worker(inner, worker);
        return;
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    match worker.completion_rx.recv_timeout(remaining) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            let handle = worker.handle.take().expect("remote worker handle");
            if handle.join().is_err() {
                set_last_connection_note(
                    inner,
                    format!("Remote worker {} panicked during shutdown.", worker.name),
                    true,
                );
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => defer_remote_worker(inner, worker),
    }
}

fn settle_web_listener(
    inner: &Arc<RemoteHostInner>,
    mut listener: WebListenerHandle,
    worker_name: &'static str,
    deadline: Instant,
) {
    // The listener reserves this permit before its runtime starts, so teardown
    // never has to compete for new admission or fall back to blocking on the
    // lifecycle stack when the global residue registry is full.
    let permit = listener.take_shutdown_permit();
    match RemoteWorker::try_spawn_with_permit(permit, worker_name, None, move || {
        listener.shutdown()
    }) {
        Ok(worker) => settle_remote_worker(inner, worker, deadline),
        Err(error) => set_last_connection_note(
            inner,
            format!("Web listener cleanup worker could not start: {error}."),
            true,
        ),
    }
}

struct RemoteHostServiceOwner {
    inner: Arc<RemoteHostInner>,
}

impl Drop for RemoteHostServiceOwner {
    fn drop(&mut self) {
        let (
            stop_generation,
            session_bootstrap_provider,
            terminal_input_handler,
            terminal_resize_handler,
            focused_session_handler,
            web_listener,
            listener_worker,
            broadcaster_worker,
        ) = {
            let _lifecycle_guard = self
                .inner
                .lifecycle_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let stop_generation = self
                .inner
                .native_runtime_generation
                .fetch_add(1, Ordering::SeqCst)
                .wrapping_add(1);
            self.inner.stop_flag.store(true, Ordering::SeqCst);
            // Wake Connect peer leases immediately on stop, even while other
            // strong Arcs still retain the inner runtime during teardown.
            bump_host_config_revision(&self.inner);
            self.inner.listener_running.store(false, Ordering::Release);
            wake_native_listener(&self.inner);
            notify_broadcaster(&self.inner);

            (
                stop_generation,
                self.inner
                    .session_bootstrap_provider
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take(),
                self.inner
                    .terminal_input_handler
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take(),
                self.inner
                    .terminal_resize_handler
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take(),
                self.inner
                    .focused_session_handler
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take(),
                self.inner
                    .web_listener
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take(),
                self.inner
                    .listener_thread
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take(),
                self.inner
                    .broadcaster_thread
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take(),
            )
        };
        cancel_native_connection_workers_before_generation(&self.inner, stop_generation);

        // Drop callbacks outside their locks. The app callbacks can retain
        // non-owning service clones (and the process manager), so running their
        // destructors while a callback lock is held could deadlock teardown.
        drop((
            session_bootstrap_provider,
            terminal_input_handler,
            terminal_resize_handler,
            focused_session_handler,
        ));

        // Revoke browser authority while the runtime can still deliver the
        // disconnect, then drain once more after shutdown to close the narrow
        // registration race between the first drain and listener teardown.
        drain_web_clients_for_restart(&self.inner);
        let deadline = Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT;

        // Web callback executors are part of the host owner even though their
        // queues are fed by async request tasks. Close both queues immediately
        // after callbacks are revoked, before a potentially stalled native or
        // broadcaster worker can consume the shared lifecycle deadline.
        let input_residue = self.inner.web_input_executor.shutdown_until(deadline);
        let request_residue = self.inner.web_request_executor.shutdown_until(deadline);
        if input_residue > 0 || request_residue > 0 {
            set_last_connection_note(
                &self.inner,
                format!(
                    "Remote web callback residue: {input_residue} input and {request_residue} request workers remain owned after bounded shutdown."
                ),
                true,
            );
            for worker in self.inner.web_input_executor.take_unfinished_workers() {
                defer_remote_worker(&self.inner, worker);
            }
            for worker in self.inner.web_request_executor.take_unfinished_workers() {
                defer_remote_worker(&self.inner, worker);
            }
        }

        if let Some(listener) = web_listener {
            settle_web_listener(&self.inner, listener, "remote-web-shutdown", deadline);
        }
        drain_web_clients_for_restart(&self.inner);

        if let Some(worker) = listener_worker {
            settle_remote_worker(&self.inner, worker, deadline);
        }
        if let Some(worker) = broadcaster_worker {
            settle_remote_worker(&self.inner, worker, deadline);
        }
        join_native_connection_workers_before_generation(&self.inner, stop_generation, deadline);
    }
}

impl Clone for RemoteHostService {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _lifetime_owner: None,
        }
    }
}

impl RemoteHostService {
    pub fn downgrade(&self) -> RemoteHostWeakHandle {
        RemoteHostWeakHandle {
            inner: Arc::downgrade(&self.inner),
        }
    }
}

/// Exact identity of one Claude hook projection attached to one PTY launch.
/// The generation prevents a late hook from an old overlay from consuming a
/// prompt submitted to a replacement Claude process that reused the PTY id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClaudeSemanticIdentity {
    pub pty_session_id: String,
    pub stable_session_key: StableSessionKey,
    pub registration_generation: u64,
}

/// Exact identity of one Codex app-server projection attached to one PTY
/// launch. The generation prevents provider events from one bridge from
/// consuming a phone prompt reserved for a replacement bridge.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CodexSemanticIdentity {
    pub pty_session_id: String,
    pub stable_session_key: StableSessionKey,
    pub registration_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposerReconciliationReservation {
    Reserved,
    NotNeeded,
    CapacityExceeded,
}

#[derive(Default)]
struct ClaudeComposerReconciliationState {
    adapters_by_pty_session: HashMap<String, ClaudeSemanticIdentity>,
    pending: VecDeque<PendingClaudeComposerPrompt>,
    reconciled_provider_keys: VecDeque<ReconciledClaudeProviderKey>,
}

struct PendingClaudeComposerPrompt {
    mutation_id: String,
    identity: ClaudeSemanticIdentity,
    text: String,
    state: PendingClaudeComposerPromptState,
    expires_at: Instant,
}

enum PendingClaudeComposerPromptState {
    Reserved {
        deferred_hook: Option<SemanticEventDraft>,
    },
    Accepted,
}

struct ReconciledClaudeProviderKey {
    identity: ClaudeSemanticIdentity,
    key: String,
    expires_at: Instant,
}

#[derive(Default)]
struct CodexComposerReconciliationState {
    adapters_by_pty_session: HashMap<String, CodexSemanticIdentity>,
    pending: VecDeque<PendingCodexComposerPrompt>,
    reconciled_provider_keys: VecDeque<ReconciledCodexProviderKey>,
}

struct PendingCodexComposerPrompt {
    mutation_id: String,
    identity: CodexSemanticIdentity,
    text: String,
    state: PendingCodexComposerPromptState,
    expires_at: Instant,
}

enum PendingCodexComposerPromptState {
    Reserved {
        deferred_provider: Option<SemanticEventDraft>,
    },
    Accepted,
}

struct ReconciledCodexProviderKey {
    identity: CodexSemanticIdentity,
    key: String,
    expires_at: Instant,
}

pub(crate) struct ListenerLease {
    port: u16,
    generation: u64,
    inner: Weak<RemoteHostInner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListenerBindFailure {
    ExternalConflict { bind: String, detail: String },
    GenerationStale { bind: String, phase: &'static str },
    Other { bind: String, detail: String },
}

impl ListenerBindFailure {
    fn from_io(bind: impl Into<String>, error: std::io::Error) -> Self {
        let bind = bind.into();
        if error.kind() == std::io::ErrorKind::AddrInUse {
            Self::ExternalConflict {
                bind,
                detail: error.to_string(),
            }
        } else {
            Self::Other {
                bind,
                detail: error.to_string(),
            }
        }
    }
}

impl std::fmt::Display for ListenerBindFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExternalConflict { bind, detail } => {
                write!(formatter, "external bind conflict on {bind}: {detail}")
            }
            Self::GenerationStale { bind, phase } => {
                write!(
                    formatter,
                    "listener generation became stale {phase} bind on {bind}"
                )
            }
            Self::Other { bind, detail } => {
                write!(formatter, "listener bind on {bind} failed: {detail}")
            }
        }
    }
}

impl ListenerLease {
    fn is_current(&self) -> bool {
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        !inner.stop_flag.load(Ordering::Acquire)
            && inner.native_runtime_generation.load(Ordering::Acquire) == self.generation
            && inner
                .listener_leases
                .lock()
                .ok()
                .and_then(|leases| leases.get(&self.port).copied())
                == Some(self.generation)
    }
}

impl Drop for ListenerLease {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if let Ok(mut leases) = inner.listener_leases.lock() {
            if leases.get(&self.port).copied() == Some(self.generation) {
                leases.remove(&self.port);
            }
        };
    }
}

fn acquire_listener_lease(
    inner: &Arc<RemoteHostInner>,
    port: u16,
    generation: u64,
) -> Result<ListenerLease, String> {
    if port == 0 {
        return Err("listener port must be non-zero".to_string());
    }
    if inner.stop_flag.load(Ordering::Acquire)
        || inner.native_runtime_generation.load(Ordering::Acquire) != generation
    {
        return Err("listener generation is no longer current".to_string());
    }
    let mut leases = inner
        .listener_leases
        .lock()
        .map_err(|_| "listener lease registry unavailable".to_string())?;
    if let Some(existing_generation) = leases.get(&port).copied() {
        return Err(format!(
            "listener port {port} is already reserved by generation {existing_generation}"
        ));
    }
    leases.insert(port, generation);
    Ok(ListenerLease {
        port,
        generation,
        inner: Arc::downgrade(inner),
    })
}

/// Host web-only path: require persisted pairing/cookie secrets. Do not mint
/// ephemeral values for an already-loaded active config without persistence.
fn require_durable_web_secrets(web: &WebConfig) -> Result<(), String> {
    if web.pairing_token.trim().is_empty() {
        return Err("durable web pairing token is missing".to_string());
    }
    let cookie_secret_is_valid = web.cookie_secret_hex.len() == 64
        && web::auth::hex_decode(&web.cookie_secret_hex).is_some_and(|secret| secret.len() == 32);
    if !cookie_secret_is_valid {
        return Err("durable web cookie signing secret is missing or invalid".to_string());
    }
    Ok(())
}

fn acquire_config_listener_leases(
    inner: &Arc<RemoteHostInner>,
    generation: u64,
    config: &RemoteHostConfig,
) -> Result<(Option<ListenerLease>, Option<ListenerLease>), String> {
    let native = if config.enabled {
        Some(acquire_listener_lease(inner, config.port, generation)?)
    } else {
        None
    };
    let web = if config.web.enabled {
        match acquire_listener_lease(inner, config.web.port, generation) {
            Ok(lease) => Some(lease),
            Err(error) => {
                drop(native);
                return Err(error);
            }
        }
    } else {
        None
    };
    Ok((native, web))
}

fn notify_broadcaster(inner: &RemoteHostInner) {
    if let Ok(mut sequence) = inner.broadcaster_signal.0.lock() {
        *sequence = sequence.wrapping_add(1);
        inner.broadcaster_signal.1.notify_all();
    }
}

fn wait_for_broadcaster_signal(signal: &Arc<(Mutex<u64>, Condvar)>, timeout: Duration) {
    let Ok(sequence) = signal.0.lock() else {
        return;
    };
    let _ = signal.1.wait_timeout(sequence, timeout);
}

fn wake_native_listener(inner: &RemoteHostInner) {
    let endpoint = inner
        .native_listener_wakeup
        .lock()
        .ok()
        .and_then(|slot| *slot);
    if let Some(endpoint) = endpoint {
        let _ = TcpStream::connect_timeout(&endpoint, Duration::from_millis(100));
    }
}

#[derive(Clone)]
pub(crate) struct RegisteredWebPushSender {
    pub(crate) listener_generation: u64,
    pub(crate) sender: web::push::PushSender,
}

pub(crate) struct RemoteHostInner {
    config: RwLock<RemoteHostConfig>,
    /// Serializes every host-config transaction, including its durable write.
    /// This is always the first authority lock for a host-config writer. A
    /// browser transaction may then take `web_control_operation_lock` and a
    /// lifecycle fence, but no path may acquire this serializer while holding
    /// either of those locks.
    host_config_tx: Mutex<()>,
    /// Serializes listener/runtime restarts without holding the config update
    /// lock across worker joins. Native workers may need that config lock while
    /// completing their disconnect cleanup.
    lifecycle_lock: Mutex<()>,
    config_revision: AtomicU64,
    /// Process-local epoch for Connect peer leases. Advanced only after a
    /// durable host-config commit (or equivalent in-memory apply) so watches
    /// observe revoke/disable without polling.
    pub(crate) host_config_watch: watch::Sender<u64>,
    /// Coordinates publication of workspace state with browser snapshot
    /// capture so a revision always describes the state sent with it.
    snapshot_state_lock: Mutex<()>,
    snapshot_revision: AtomicU64,
    runtime_instance_id: String,
    shared_state: RwLock<AppState>,
    runtime_state: RwLock<RuntimeState>,
    port_statuses: RwLock<HashMap<u16, PortStatus>>,
    port_authorities: RwLock<HashMap<u16, RemotePortAuthority>>,
    /// Task3.4 supplies this exact registry snapshot when the host can prove
    /// a managed forwarding request. Empty is intentional until that union
    /// seam is wired; it makes every managed wire label fail closed.
    managed_port_snapshots: RwLock<HashMap<u16, Arc<ManagedResourceCapability>>>,
    semantic_journals: Mutex<SemanticJournalStore>,
    /// Serializes semantic writers while the generation below gives browser
    /// capture a lock-free indication that publication is in progress.
    semantic_publication_lock: Mutex<()>,
    semantic_publication_generation: AtomicU64,
    #[cfg(test)]
    semantic_publication_test_hook: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Serializes browser subscription commits and broadcaster delivery. It is
    /// intentionally separate from semantic publication, so replay cloning or
    /// a slow browser can never block the PTY output path.
    semantic_delivery_lock: Mutex<()>,
    #[cfg(test)]
    semantic_delivery_test_hook: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    port_forward_connector_test_hook:
        RwLock<Option<Arc<dyn Fn(u16) -> Result<TcpStream, String> + Send + Sync>>>,
    #[cfg(test)]
    lifecycle_lock_acquired_test_hook: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    worker_reaped_test_hook: RwLock<Option<mpsc::SyncSender<RemoteWorkerReapedEvent>>>,
    #[cfg(test)]
    native_lifecycle_test_hook: RwLock<Option<Arc<dyn Fn(NativeLifecycleTestEvent) + Send + Sync>>>,
    #[cfg(test)]
    native_worker_registration_test_hook: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    client_registration_test_hook:
        RwLock<Option<Arc<dyn Fn(ClientRegistrationTestEvent) + Send + Sync>>>,
    #[cfg(test)]
    browser_admission_clock_test_hook: RwLock<Option<Arc<dyn Fn() -> u64 + Send + Sync>>>,
    /// Non-blocking admission handle for the web listener's bounded Push
    /// delivery pool. It is absent whenever the listener is stopped.
    web_push_sender: RwLock<Option<RegisteredWebPushSender>>,
    session_bootstrap_provider: RwLock<Option<SessionBootstrapProvider>>,
    terminal_input_handler: RwLock<Option<TerminalInputHandler>>,
    terminal_resize_handler: RwLock<Option<TerminalResizeHandler>>,
    focused_session_handler: RwLock<Option<FocusedSessionHandler>>,
    /// Serializes browser control transitions and the Resume capture/enqueue
    /// sequence. It is never held while a terminal/bootstrap callback runs.
    web_control_operation_lock: Mutex<()>,
    /// Browser writer leases, exact legacy claimant, deferred takeover, and
    /// busy composer state share one reducer so no path can invalidate only
    /// part of the authority state.
    web_control: Mutex<WebControlState>,
    web_composer_mutations: Mutex<HashMap<String, WebComposerMutationRecord>>,
    web_input_executor: WebInputExecutor,
    web_request_executor: WebRequestExecutor,
    host_work_limiter: RemoteHostWorkLimiter,
    claude_composer_reconciliation: Mutex<ClaudeComposerReconciliationState>,
    codex_composer_reconciliation: Mutex<CodexComposerReconciliationState>,
    pending_requests: Mutex<Vec<PendingRemoteRequest>>,
    clients: Mutex<HashMap<u64, ConnectedRemoteClient>>,
    controller_client_id: RwLock<Option<String>>,
    listener_running: AtomicBool,
    listener_error: RwLock<Option<String>>,
    last_connection_note: RwLock<Option<String>>,
    last_connection_is_error: AtomicBool,
    latency: RwLock<RemoteLatencyStats>,
    next_connection_id: AtomicU64,
    next_output_chunk_seq: AtomicU64,
    next_push_event_id: AtomicU64,
    next_host_config_attempt_id: AtomicU64,
    native_runtime_generation: AtomicU64,
    stop_flag: AtomicBool,
    worker_residue_count: AtomicUsize,
    listener_thread: Mutex<Option<RemoteWorker>>,
    broadcaster_thread: Mutex<Option<RemoteWorker>>,
    listener_leases: Mutex<HashMap<u16, u64>>,
    native_listener_wakeup: Mutex<Option<SocketAddr>>,
    /// Kept separately shareable so a deferred broadcaster can wait without
    /// retaining the host runtime it is meant to let tear down.
    broadcaster_signal: Arc<(Mutex<u64>, Condvar)>,
    native_connection_workers: Mutex<HashMap<u64, NativeConnectionWorker>>,
    // Both fields are written on lifecycle transitions and (Phase 1b+)
    // surfaced through the settings panel; suppress the transient warning.
    #[allow(dead_code)]
    web_listener: Mutex<Option<WebListenerHandle>>,
    #[allow(dead_code)]
    web_listener_error: RwLock<Option<String>>,
    /// Fail-closed: raw PTY/session-stream cannot leave the host unless a
    /// test explicitly disables this gate. Production Connect uses `/api/connect`.
    connect_encryption_required: AtomicBool,
    connect_startup_error: RwLock<Option<String>>,
    connect_listener_bound: AtomicBool,
    /// Host-owned auth/config shell: never execute legacy native TCP listener or
    /// snapshot broadcaster. Persisted `config.enabled` is left unchanged.
    web_only_execution: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct WebComposerMutationRecord {
    pub(crate) fingerprint: u64,
    pub(crate) status: WebComposerMutationStatus,
}

#[derive(Debug, Clone)]
pub(crate) enum WebComposerMutationStatus {
    InFlight,
    PtyRejected {
        message: String,
    },
    Accepted {
        stable_session_key: StableSessionKey,
        accepted_sequence: u64,
        lease_generation: u64,
    },
}

struct SemanticPublicationEpoch<'a> {
    generation: &'a AtomicU64,
}

impl Drop for SemanticPublicationEpoch<'_> {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Release);
    }
}

#[derive(Clone)]
struct ConnectedRemoteClient {
    client_id: String,
    /// Arc identity is the native registration token. A stale sender failure
    /// may remove only the exact map entry from which that sender was cloned,
    /// never a newer registration that reused the connection id.
    sender: Option<Arc<mpsc::Sender<ServerMessage>>>,
    /// Present only for browser clients. Browser-only semantic/control frames
    /// must never enter the native MessagePack `ServerMessage` protocol.
    web_sender: Option<BrowserOutboundSender>,
    web_tombstone: Option<Arc<WebConnectionTombstone>>,
    semantic_cursors: HashMap<StableSessionKey, u64>,
    subscribed_session_ids: HashSet<String>,
    bootstrapped_session_ids: HashSet<String>,
    bootstrap_pending_session_ids: HashSet<String>,
    focused_session_id: Option<String>,
    last_app_hash: u64,
    last_runtime_hash: u64,
    last_port_hash: u64,
    last_controller_client_id: Option<String>,
    last_you_have_control: bool,
    last_snapshot_revision: u64,
}

#[derive(Clone)]
enum ClientDeliveryTarget {
    Native(Arc<mpsc::Sender<ServerMessage>>),
    Browser {
        sender: BrowserOutboundSender,
        client_id: String,
        tombstone: Arc<WebConnectionTombstone>,
    },
}

fn client_delivery_target(client: &ConnectedRemoteClient) -> Option<ClientDeliveryTarget> {
    if let (Some(sender), Some(tombstone)) =
        (client.web_sender.clone(), client.web_tombstone.clone())
    {
        return Some(ClientDeliveryTarget::Browser {
            sender,
            client_id: client.client_id.clone(),
            tombstone,
        });
    }
    client.sender.clone().map(ClientDeliveryTarget::Native)
}

fn deliver_server_message(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    target: &ClientDeliveryTarget,
    message: ServerMessage,
) -> bool {
    match target {
        ClientDeliveryTarget::Native(sender) => sender.send(message).is_ok(),
        ClientDeliveryTarget::Browser {
            sender, client_id, ..
        } => sender
            .try_send_server_message(&message, inner, connection_id, client_id)
            .is_ok(),
    }
}

fn revoke_failed_delivery(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    target: ClientDeliveryTarget,
) {
    match target {
        ClientDeliveryTarget::Browser {
            client_id,
            tombstone,
            ..
        } => {
            web::bridge::revoke_web_connection(inner, connection_id, &client_id, &tombstone, None);
        }
        ClientDeliveryTarget::Native(sender) => {
            remove_exact_native_registration(inner, connection_id, &sender);
        }
    }
}

fn remove_exact_native_registration(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    sender: &Arc<mpsc::Sender<ServerMessage>>,
) -> bool {
    inner
        .clients
        .lock()
        .map(|mut clients| {
            let exact = clients
                .get(&connection_id)
                .and_then(|client| client.sender.as_ref())
                .is_some_and(|registered| Arc::ptr_eq(registered, sender));
            exact && clients.remove(&connection_id).is_some()
        })
        .unwrap_or(false)
}

#[derive(Clone)]
pub struct RemoteClientHandle {
    inner: Arc<RemoteClientInner>,
    connection: Arc<RemoteClientConnectionOwner>,
}

fn sync_screen_snapshot_dimensions(
    screen: &mut TerminalScreenSnapshot,
    dimensions: SessionDimensions,
) {
    screen.rows = dimensions.rows as usize;
    screen.cols = dimensions.cols as usize;
    screen.history_size = screen.total_lines.saturating_sub(screen.rows);
    screen.display_offset = screen.display_offset.min(screen.history_size);
}

struct RemoteClientInner {
    pending: Mutex<HashMap<u64, mpsc::Sender<RemoteActionResult>>>,
    next_request_id: AtomicU64,
    latest_snapshot: RwLock<Option<RemoteWorkspaceSnapshot>>,
    session_replicas: RwLock<HashMap<String, TerminalReplica>>,
    disconnected_message: RwLock<Option<String>>,
    snapshot_revision: AtomicU64,
    session_stream_revision: AtomicU64,
    latency: RwLock<RemoteLatencyStats>,
    pending_paint_received_at_epoch_ms: AtomicU64,
    pending_notification_count: AtomicU64,
    client_id: String,
    client_token: String,
    server_id: String,
    certificate_fingerprint: String,
    address: String,
    port: u16,
    #[cfg(test)]
    reader_exit_test_hook: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
}

struct RemoteClientConnectionOwner {
    outgoing: mpsc::Sender<ClientMessage>,
    socket_wakeup: Mutex<Option<TcpStream>>,
    reader: Mutex<Option<RemoteWorker>>,
    inner: Weak<RemoteClientInner>,
}

impl Drop for RemoteClientConnectionOwner {
    fn drop(&mut self) {
        if let Ok(mut socket) = self.socket_wakeup.lock() {
            if let Some(socket) = socket.take() {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
        let _ = self.outgoing.send(ClientMessage::Disconnect);
        let reader = self
            .reader
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(reader) = reader {
            settle_remote_client_worker(
                self.inner.upgrade(),
                reader,
                Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT,
            );
        }
    }
}

pub struct LocalPortForwardManager {
    inner: Arc<LocalPortForwardManagerInner>,
}

struct LocalPortForwardManagerInner {
    client: RemoteClientHandle,
    manager_handle_count: AtomicUsize,
    operation_lock: Mutex<()>,
    entries: Mutex<HashMap<u16, LocalPortForwardEntry>>,
    worker_registry: Mutex<LocalPortForwardWorkerRegistry>,
    next_scope_id: AtomicU64,
    next_connection_id: AtomicU64,
    worker_residue_count: AtomicUsize,
    statuses: RwLock<HashMap<u16, RemotePortForwardState>>,
    #[cfg(test)]
    connection_handler_test_hook:
        RwLock<Option<Arc<dyn Fn(u16, TcpStream, Arc<AtomicBool>) + Send + Sync>>>,
    #[cfg(test)]
    lifecycle_test_hook:
        RwLock<Option<Arc<dyn Fn(LocalPortForwardLifecycleTestEvent) + Send + Sync>>>,
}

struct LocalPortForwardEntry {
    scope_id: Option<u64>,
    stop: Option<Arc<AtomicBool>>,
    worker: Option<RemoteWorker>,
    wakeup: Option<SocketAddr>,
    retry_after_epoch_ms: u64,
}

struct LocalPortForwardConnectionWorker {
    port: u16,
    scope_id: u64,
    cancellation: Arc<ForwardCancellation>,
    worker: RemoteWorker,
}

#[derive(Default)]
struct LocalPortForwardWorkerRegistry {
    active_scopes: HashMap<u16, u64>,
    connections: HashMap<u64, LocalPortForwardConnectionWorker>,
}

#[cfg(test)]
impl LocalPortForwardWorkerRegistry {
    fn is_empty(&self) -> bool {
        self.active_scopes.is_empty() && self.connections.is_empty()
    }
}

impl RemoteHostService {
    pub fn new(config: RemoteHostConfig) -> Self {
        let service = Self::construct(config, false);
        service.apply_config(service.config());
        service
    }

    /// Auth/config shell for durable `devmanager-host` Connect ownership.
    ///
    /// Stores the supplied host config without minting pairing/cookie secrets or
    /// TLS material. Missing durable web secrets fail closed. Never starts the
    /// legacy native TCP listener or snapshot broadcaster, even when persisted
    /// `config.enabled` is true. The caller owns web-listener start via
    /// [`Self::start_web_listener_for_host`]; this constructor does not bind
    /// ports, write `remote.json`, auto-enable, or auto-enroll.
    pub fn new_web_only(config: RemoteHostConfig) -> Result<Self, String> {
        require_durable_web_secrets(&config.web)?;
        Ok(Self::construct_stored(config, true))
    }

    fn construct(config: RemoteHostConfig, web_only_execution: bool) -> Self {
        let mut config = config;
        config.web.ensure_secrets();
        let _ = transport::ensure_host_tls_material(&mut config);
        Self::construct_stored(config, web_only_execution)
    }

    fn construct_stored(config: RemoteHostConfig, web_only_execution: bool) -> Self {
        let (host_config_watch, _) = watch::channel(1_u64);
        let inner = Arc::new(RemoteHostInner {
            config: RwLock::new(config),
            host_config_tx: Mutex::new(()),
            lifecycle_lock: Mutex::new(()),
            config_revision: AtomicU64::new(1),
            host_config_watch,
            snapshot_state_lock: Mutex::new(()),
            snapshot_revision: AtomicU64::new(1),
            runtime_instance_id: generate_secret("runtime"),
            shared_state: RwLock::new(AppState::default()),
            runtime_state: RwLock::new(RuntimeState::default()),
            port_statuses: RwLock::new(HashMap::new()),
            port_authorities: RwLock::new(HashMap::new()),
            managed_port_snapshots: RwLock::new(HashMap::new()),
            semantic_journals: Mutex::new(SemanticJournalStore::default()),
            semantic_publication_lock: Mutex::new(()),
            semantic_publication_generation: AtomicU64::new(0),
            #[cfg(test)]
            semantic_publication_test_hook: RwLock::new(None),
            semantic_delivery_lock: Mutex::new(()),
            #[cfg(test)]
            semantic_delivery_test_hook: RwLock::new(None),
            #[cfg(test)]
            port_forward_connector_test_hook: RwLock::new(None),
            #[cfg(test)]
            lifecycle_lock_acquired_test_hook: RwLock::new(None),
            #[cfg(test)]
            worker_reaped_test_hook: RwLock::new(None),
            #[cfg(test)]
            native_lifecycle_test_hook: RwLock::new(None),
            #[cfg(test)]
            native_worker_registration_test_hook: RwLock::new(None),
            #[cfg(test)]
            client_registration_test_hook: RwLock::new(None),
            #[cfg(test)]
            browser_admission_clock_test_hook: RwLock::new(None),
            web_push_sender: RwLock::new(None),
            session_bootstrap_provider: RwLock::new(None),
            terminal_input_handler: RwLock::new(None),
            terminal_resize_handler: RwLock::new(None),
            focused_session_handler: RwLock::new(None),
            web_control_operation_lock: Mutex::new(()),
            web_control: Mutex::new(WebControlState::new(Duration::from_secs(8))),
            web_composer_mutations: Mutex::new(HashMap::new()),
            web_input_executor: WebInputExecutor::default(),
            web_request_executor: WebRequestExecutor::default(),
            host_work_limiter: RemoteHostWorkLimiter::new(MAX_CONCURRENT_REMOTE_HOST_WORK),
            claude_composer_reconciliation: Mutex::new(ClaudeComposerReconciliationState::default()),
            codex_composer_reconciliation: Mutex::new(CodexComposerReconciliationState::default()),
            pending_requests: Mutex::new(Vec::new()),
            clients: Mutex::new(HashMap::new()),
            controller_client_id: RwLock::new(None),
            listener_running: AtomicBool::new(false),
            listener_error: RwLock::new(None),
            last_connection_note: RwLock::new(None),
            last_connection_is_error: AtomicBool::new(false),
            latency: RwLock::new(RemoteLatencyStats::default()),
            next_connection_id: AtomicU64::new(1),
            next_output_chunk_seq: AtomicU64::new(1),
            next_push_event_id: AtomicU64::new(1),
            next_host_config_attempt_id: AtomicU64::new(1),
            native_runtime_generation: AtomicU64::new(1),
            stop_flag: AtomicBool::new(false),
            worker_residue_count: AtomicUsize::new(0),
            listener_thread: Mutex::new(None),
            broadcaster_thread: Mutex::new(None),
            listener_leases: Mutex::new(HashMap::new()),
            native_listener_wakeup: Mutex::new(None),
            broadcaster_signal: Arc::new((Mutex::new(0), Condvar::new())),
            native_connection_workers: Mutex::new(HashMap::new()),
            web_listener: Mutex::new(None),
            web_listener_error: RwLock::new(None),
            connect_encryption_required: AtomicBool::new(true),
            connect_startup_error: RwLock::new(None),
            connect_listener_bound: AtomicBool::new(false),
            web_only_execution,
        });
        let service = Self {
            _lifetime_owner: Some(RemoteHostServiceOwner {
                inner: inner.clone(),
            }),
            inner,
        };
        service.install_connect_production_gate();
        service
    }

    /// Bind the Connect web listener for host-owned lifetime.
    ///
    /// No-op when `config.web.enabled` is false. Never starts legacy native
    /// listener/broadcaster workers. Does not persist config. Connect bind
    /// status is taken from the started handle's own production startup, not a
    /// second factory call.
    pub(crate) fn start_web_listener_for_host(&self) -> Result<(), String> {
        if !self.inner.web_only_execution {
            return Err("start_web_listener_for_host requires web-only mode".to_string());
        }
        let config = self.config();
        if !config.web.enabled {
            return Ok(());
        }
        let generation = self.inner.native_runtime_generation.load(Ordering::Acquire);
        let lease = acquire_listener_lease(&self.inner, config.web.port, generation)?;
        match WebListenerHandle::start(self.inner.clone(), config.web.clone(), lease) {
            Ok(handle) => {
                handle.publish_push_sender();
                let connect_startup_present = handle.connect_startup_present();
                let connect_bound = handle.require_connect_startup_bound();
                {
                    let _lifecycle_guard = self
                        .inner
                        .lifecycle_lock
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if self.inner.stop_flag.load(Ordering::Acquire) {
                        drop(_lifecycle_guard);
                        // Direct shutdown on the host OS worker path — do not defer
                        // to the legacy residue registry for host-owned lifetime.
                        handle.shutdown();
                        return Err("web listener generation stopped before install".to_string());
                    }
                    self.inner
                        .connect_encryption_required
                        .store(true, Ordering::Release);
                    match &connect_bound {
                        Ok(()) => {
                            self.inner
                                .connect_listener_bound
                                .store(true, Ordering::Release);
                            surface_connect_startup(&self.inner, None, false);
                        }
                        Err(error) => {
                            self.inner
                                .connect_listener_bound
                                .store(false, Ordering::Release);
                            // Missing startup is the held-closed prepare path (e.g.
                            // unenrolled). Present-but-unbound is unexpected.
                            surface_connect_startup(
                                &self.inner,
                                Some(error.clone()),
                                connect_startup_present,
                            );
                        }
                    }
                    *self
                        .inner
                        .web_listener
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
                }
                Ok(())
            }
            Err(error) => {
                if let Ok(mut error_slot) = self.inner.web_listener_error.write() {
                    *error_slot = Some(error.to_string());
                }
                self.inner
                    .connect_listener_bound
                    .store(false, Ordering::Release);
                surface_connect_startup(
                    &self.inner,
                    Some(format!("web listener bind failed: {error}")),
                    true,
                );
                Err(error.to_string())
            }
        }
    }

    /// Host-owned web-only shutdown: take the listener and call
    /// [`WebListenerHandle::shutdown`] directly on this OS thread, then drop the
    /// service. Does not claim success via residue-deferred Owner drop.
    pub(crate) fn shutdown_web_listener_for_host(self) -> Result<(), String> {
        if !self.inner.web_only_execution {
            return Err("shutdown_web_listener_for_host requires web-only mode".to_string());
        }
        let handle = {
            let _lifecycle_guard = self
                .inner
                .lifecycle_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.inner
                .native_runtime_generation
                .fetch_add(1, Ordering::SeqCst);
            self.inner.stop_flag.store(true, Ordering::SeqCst);
            bump_host_config_revision(&self.inner);
            self.inner
                .web_listener
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
        };
        if let Some(handle) = handle {
            handle.shutdown();
        }
        self.inner
            .connect_listener_bound
            .store(false, Ordering::Release);
        // Dropping self runs Owner cleanup with web_listener already taken, so
        // legacy residue settle is not used for this host-owned listener.
        drop(self);
        Ok(())
    }

    pub(crate) fn web_only_execution(&self) -> bool {
        self.inner.web_only_execution
    }

    pub(crate) fn web_listener_is_installed(&self) -> bool {
        self.inner
            .web_listener
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
    }

    /// Fail-closed production gate. Legacy `/api/ws` is never Connect and
    /// cannot emit raw PTY/session-stream unless a test setter disables this.
    pub fn install_connect_production_gate(&self) {
        self.inner
            .connect_encryption_required
            .store(true, Ordering::Release);
        self.inner
            .connect_listener_bound
            .store(false, Ordering::Release);
        let _ = crate::connect::ConnectProductionStartup::reject_legacy_remote_web_as_connect();
        surface_connect_startup(
            &self.inner,
            Some(crate::connect::ConnectStartupError::ListenerNotBound.to_string()),
            false,
        );
    }

    pub fn connect_listener_kind(&self) -> crate::connect::ConnectListenerKind {
        crate::connect::ConnectListenerKind::LegacyRemoteWeb
    }

    pub fn connect_encryption_required(&self) -> bool {
        self.inner
            .connect_encryption_required
            .load(Ordering::Acquire)
    }

    pub fn connect_startup_error(&self) -> Option<String> {
        self.inner
            .connect_startup_error
            .read()
            .ok()
            .and_then(|slot| slot.clone())
    }

    /// Open OS-backed Connect production identity/custody. Never claims a
    /// listener is bound. Unenrolled/pending/revoked and custody failures are
    /// written to host status/notes and keep Connect fail-closed.
    pub fn prepare_connect_production_or_surface(&self) {
        self.inner
            .connect_encryption_required
            .store(true, Ordering::Release);
        self.inner
            .connect_listener_bound
            .store(false, Ordering::Release);
        match crate::connect::ConnectProductionStartup::prepare_direct(
            crate::connect::DirectBindPolicy::loopback(),
        ) {
            Ok(startup) => {
                let _ = startup.require_bound_listener();
                surface_connect_startup(
                    &self.inner,
                    Some(crate::connect::ConnectStartupError::ListenerNotBound.to_string()),
                    false,
                );
            }
            Err(error) if error.is_unenrolled_identity() => {
                surface_connect_startup(&self.inner, Some(error.to_string()), false);
            }
            Err(error) => {
                surface_connect_startup(&self.inner, Some(error.to_string()), true);
            }
        }
    }

    pub(crate) fn mark_connect_listener_bound(&self, bound: bool) {
        self.inner
            .connect_listener_bound
            .store(bound, Ordering::Release);
        if bound {
            self.inner
                .connect_encryption_required
                .store(true, Ordering::Release);
            surface_connect_startup(&self.inner, None, false);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_connect_encryption_required_for_test(&self, required: bool) {
        self.inner
            .connect_encryption_required
            .store(required, Ordering::Release);
    }

    pub(crate) fn borrowed(inner: Arc<RemoteHostInner>) -> Self {
        Self {
            inner,
            _lifetime_owner: None,
        }
    }

    pub(crate) fn web_mutation_authority_is_current(
        &self,
        authority: &RemoteWebMutationAuthority,
    ) -> bool {
        web::bridge::web_mutation_authority_is_current(&self.inner, authority)
    }

    pub(crate) fn try_acquire_work_permit(&self) -> Option<RemoteHostWorkPermit> {
        self.inner.host_work_limiter.try_acquire()
    }

    pub fn apply_config(&self, config: RemoteHostConfig) {
        let mut config = config;
        config.web.ensure_secrets();
        let _ = transport::ensure_host_tls_material(&mut config);
        {
            let Ok(_update_guard) = self.inner.host_config_tx.lock() else {
                return;
            };
            if let Ok(mut slot) = self.inner.config.write() {
                *slot = config;
                // Notify after the in-memory mutation is visible to readers.
                self.bump_config_revision();
            }
        }
        self.restart_threads();
    }

    pub fn update_native_listener_settings(
        &self,
        enabled: bool,
        bind_address: String,
        port: u16,
    ) -> Result<(), String> {
        let bind_address = bind_address.trim().to_string();
        if bind_address.is_empty() {
            return Err("Native bind address is required".to_string());
        }
        if port == 0 {
            return Err("Native port must be between 1 and 65535".to_string());
        }
        let changed = mutate_host_config_if(
            &self.inner,
            |config| {
                config.enabled != enabled
                    || config.bind_address != bind_address
                    || config.port != port
            },
            |config| {
                config.enabled = enabled;
                config.bind_address = bind_address.clone();
                config.port = port;
            },
        )
        .map_err(|error| error.to_string())?
        .is_some();
        if changed {
            self.restart_threads();
        }
        Ok(())
    }

    pub fn update_web_listener_settings(
        &self,
        enabled: bool,
        bind_address: String,
        port: u16,
    ) -> Result<(), String> {
        let bind_address = bind_address.trim().to_string();
        if bind_address.is_empty() {
            return Err("Browser bind address is required".to_string());
        }
        if port == 0 {
            return Err("Browser port must be between 1 and 65535".to_string());
        }
        let changed = mutate_host_config_if(
            &self.inner,
            |config| {
                config.web.enabled != enabled
                    || config.web.bind_address != bind_address
                    || config.web.port != port
            },
            |config| {
                config.web.enabled = enabled;
                config.web.bind_address = bind_address.clone();
                config.web.port = port;
                config.web.ensure_secrets();
            },
        )
        .map_err(|error| error.to_string())?
        .is_some();
        if changed {
            self.restart_threads();
        }
        Ok(())
    }

    pub fn regenerate_native_pairing_token(&self) -> Result<String, String> {
        let token = generate_pairing_token();
        mutate_host_config(&self.inner, |config| {
            config.pairing_token = token.clone();
        })
        .map_err(|error| error.to_string())?;
        Ok(token)
    }

    pub fn regenerate_web_pairing_token(&self) -> Result<String, String> {
        let token = web::generate_web_pairing_token();
        mutate_host_config(&self.inner, |config| {
            config.web.pairing_token = token.clone();
        })
        .map_err(|error| error.to_string())?;
        Ok(token)
    }

    pub fn update_snapshot(
        &self,
        app_state: AppState,
        runtime_state: RuntimeState,
        port_statuses: HashMap<u16, PortStatus>,
    ) {
        self.update_snapshot_parts(
            Some(app_state),
            Some(runtime_state),
            Some(port_statuses),
            Some(HashMap::new()),
        );
    }

    /// Inject the current Task3.4 registry authority for forwarding. The
    /// normal host path leaves this empty until the registry handoff is
    /// available; tests and the eventual union adapter provide exact,
    /// independently reconciled snapshots here.
    pub(crate) fn update_managed_port_capabilities(
        &self,
        snapshots: HashMap<u16, Arc<ManagedResourceCapability>>,
    ) {
        if let Ok(mut slot) = self.inner.managed_port_snapshots.write() {
            *slot = snapshots;
        }
    }

    pub fn update_snapshot_parts(
        &self,
        app_state: Option<AppState>,
        runtime_state: Option<RuntimeState>,
        port_statuses: Option<HashMap<u16, PortStatus>>,
        port_authorities: Option<HashMap<u16, RemotePortAuthority>>,
    ) {
        self.update_snapshot_parts_with_authorities(
            app_state,
            runtime_state,
            port_statuses,
            port_authorities,
        );
    }

    pub fn update_snapshot_parts_with_authorities(
        &self,
        app_state: Option<AppState>,
        runtime_state: Option<RuntimeState>,
        port_statuses: Option<HashMap<u16, PortStatus>>,
        port_authorities: Option<HashMap<u16, RemotePortAuthority>>,
    ) {
        let semantic_inputs_changed = app_state.is_some() || runtime_state.is_some();
        let _snapshot_guard = self
            .inner
            .snapshot_state_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if runtime_state.is_some() {
            // Runtime/session generations are part of the managed capability
            // predicate. A runtime publication invalidates every prior
            // capability; the registry union must issue a fresh one after
            // publishing the matching runtime generation.
            self.update_managed_port_capabilities(HashMap::new());
        }
        let mut changed = false;
        if let Some(app_state) = app_state {
            if let Ok(mut slot) = self.inner.shared_state.write() {
                *slot = app_state;
                changed = true;
            }
        }
        if let Some(runtime_state) = runtime_state {
            if let Ok(mut slot) = self.inner.runtime_state.write() {
                *slot = runtime_state;
                changed = true;
            }
        }
        if let Some(port_statuses) = port_statuses {
            if let Ok(mut slot) = self.inner.port_statuses.write() {
                *slot = port_statuses;
                changed = true;
            }
        }
        if let Some(port_authorities) = port_authorities {
            if let Ok(mut slot) = self.inner.port_authorities.write() {
                *slot = port_authorities;
                changed = true;
            }
        }
        if semantic_inputs_changed {
            let tabs = self
                .inner
                .shared_state
                .read()
                .map(|state| state.open_tabs.clone())
                .unwrap_or_default();
            let sessions = self
                .inner
                .runtime_state
                .read()
                .map(|runtime| runtime.sessions.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let occurred_at_epoch_ms = now_epoch_ms();
            if let Ok(mut journals) = self.inner.semantic_journals.lock() {
                for session in &sessions {
                    changed |= journals.observe_runtime(session, &tabs, occurred_at_epoch_ms);
                }
            }
        }
        if changed {
            self.inner.snapshot_revision.fetch_add(1, Ordering::Relaxed);
            notify_broadcaster(&self.inner);
        }
    }

    pub fn config(&self) -> RemoteHostConfig {
        self.inner
            .config
            .read()
            .map(|slot| slot.clone())
            .unwrap_or_default()
    }

    pub fn config_revision(&self) -> u64 {
        self.inner.config_revision.load(Ordering::Relaxed)
    }

    pub fn semantic_replay(&self, key: &StableSessionKey, cursor: u64) -> Option<SemanticReplay> {
        self.inner
            .semantic_journals
            .lock()
            .ok()
            .and_then(|journals| journals.replay_after(key, cursor))
    }

    pub fn semantic_session_metadata(
        &self,
        key: &StableSessionKey,
    ) -> Option<SemanticSessionMetadata> {
        self.inner
            .semantic_journals
            .lock()
            .ok()
            .and_then(|journals| journals.metadata(key))
    }

    pub fn set_session_bootstrap_provider(&self, provider: Option<SessionBootstrapProvider>) {
        if let Ok(mut slot) = self.inner.session_bootstrap_provider.write() {
            *slot = provider;
        }
    }

    pub fn set_terminal_input_handler(&self, handler: Option<TerminalInputHandler>) {
        if let Ok(mut slot) = self.inner.terminal_input_handler.write() {
            *slot = handler;
        }
    }

    pub fn set_terminal_resize_handler(&self, handler: Option<TerminalResizeHandler>) {
        if let Ok(mut slot) = self.inner.terminal_resize_handler.write() {
            *slot = handler;
        }
    }

    pub fn set_focused_session_handler(&self, handler: Option<FocusedSessionHandler>) {
        if let Ok(mut slot) = self.inner.focused_session_handler.write() {
            *slot = handler;
        }
    }

    pub fn record_input_write_latency(&self, enqueued_at_epoch_ms: u64) {
        let elapsed_ms = now_epoch_ms().saturating_sub(enqueued_at_epoch_ms);
        if let Ok(mut latency) = self.inner.latency.write() {
            latency.input_enqueue_to_host_write_ms = Some(elapsed_ms);
        }
    }

    pub fn subscribed_session_ids(&self) -> HashSet<String> {
        self.inner
            .clients
            .lock()
            .map(|clients| {
                clients
                    .values()
                    .flat_map(|client| client.subscribed_session_ids.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn mark_subscribed_clients_bootstrap_pending(&self, session_id: &str) {
        let Ok(mut clients) = self.inner.clients.lock() else {
            return;
        };
        for client in clients.values_mut() {
            if client.subscribed_session_ids.contains(session_id)
                && !client.bootstrapped_session_ids.contains(session_id)
            {
                // Only mark the session pending here. Doing the actual
                // bootstrap lookup inline from `push_session_output()` used to
                // block live AI output behind a heavy PTY snapshot, which left
                // the web terminal black and amplified native hangs when the
                // same session was selected locally.
                client
                    .bootstrap_pending_session_ids
                    .insert(session_id.to_string());
            }
        }
    }

    pub fn push_semantic_draft(&self, draft: SemanticEventDraft) {
        let visibility_guard = self
            .inner
            .web_control_operation_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stable_session_key = draft.stable_session_key.clone();
        let provider = draft.source;
        let is_ai_provider = matches!(provider, SemanticSource::Claude | SemanticSource::Codex);
        let is_question =
            is_ai_provider && matches!(&draft.kind, SemanticEventKind::Question { .. });
        let is_completion = match &draft.kind {
            SemanticEventKind::Status { state, .. } if is_ai_provider => {
                let state = state.trim().to_ascii_lowercase();
                matches!(
                    state.as_str(),
                    "completed" | "complete" | "done" | "success"
                ) || (provider == SemanticSource::Claude && state == "ready")
                    || (provider == SemanticSource::Codex && state == "idle")
            }
            _ => false,
        };
        let mut push_action = None;
        let changed = self.publish_semantic_change(|journals| {
            let previous = journals.metadata(&stable_session_key);
            journals.record(draft);
            if is_completion
                && previous
                    .as_ref()
                    .is_none_or(|metadata| metadata.attention == SemanticAttention::None)
            {
                journals.set_attention(&stable_session_key, SemanticAttention::Unread, 1);
            }
            let current = journals.metadata(&stable_session_key);
            if is_question
                && previous.as_ref().map(|metadata| metadata.attention)
                    != Some(SemanticAttention::NeedsInput)
                && current.as_ref().map(|metadata| metadata.attention)
                    == Some(SemanticAttention::NeedsInput)
            {
                push_action = Some(web::push::PushAttentionKind::NeedsInput);
            } else if is_completion
                && previous.as_ref().map(|metadata| metadata.attention)
                    != Some(SemanticAttention::Unread)
                && current.as_ref().map(|metadata| metadata.attention)
                    == Some(SemanticAttention::Unread)
            {
                push_action = Some(web::push::PushAttentionKind::Completed);
            }
            true
        });
        if let Some(action) = push_action {
            self.enqueue_push_attention(None, &stable_session_key, action);
        }
        drop(visibility_guard);
        if changed {
            let _ = deliver_live_semantic_events(&self.inner);
        }
    }

    pub fn push_session_output(&self, session_id: &str, bytes: Vec<u8>) {
        self.push_session_output_inner(session_id, bytes, None, None);
    }

    pub fn push_claude_adapter_registered(&self, identity: ClaudeSemanticIdentity) {
        let deferred = {
            let mut state = self
                .inner
                .claude_composer_reconciliation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let deferred = drain_expired_claude_reconciliations(&mut state, Instant::now());
            state
                .adapters_by_pty_session
                .insert(identity.pty_session_id.clone(), identity);
            deferred
        };
        for draft in deferred {
            self.push_semantic_draft(draft);
        }
    }

    pub fn push_claude_adapter_removed(&self, identity: &ClaudeSemanticIdentity) {
        let deferred = {
            let mut state = self
                .inner
                .claude_composer_reconciliation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let deferred = drain_expired_claude_reconciliations(&mut state, Instant::now());
            if state.adapters_by_pty_session.get(&identity.pty_session_id) == Some(identity) {
                state
                    .adapters_by_pty_session
                    .remove(&identity.pty_session_id);
            }
            // Adapter lifetime and composer-write lifetime can cross: the PTY
            // callback may still accept a write after its exact adapter exits
            // or is replaced. Keep generation-scoped reservations and retry
            // keys until accept/cancel or their bounded TTL resolves them.
            deferred
        };
        for draft in deferred {
            self.push_semantic_draft(draft);
        }
    }

    pub fn push_claude_semantic_draft(
        &self,
        identity: ClaudeSemanticIdentity,
        draft: SemanticEventDraft,
    ) {
        enum Decision {
            Publish,
            Reconciled,
        }

        let (expired, decision) = {
            let mut state = self
                .inner
                .claude_composer_reconciliation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let expired = drain_expired_claude_reconciliations(&mut state, Instant::now());
            let provider_key_reconciled = draft.deduplication_key.as_ref().is_some_and(|key| {
                state
                    .reconciled_provider_keys
                    .iter()
                    .any(|entry| entry.identity == identity && entry.key == *key)
            });
            let text = match &draft.kind {
                presentation::SemanticEventKind::UserMessage { text } => Some(text.as_str()),
                _ => None,
            };
            let mut decision = Decision::Publish;
            if provider_key_reconciled {
                decision = Decision::Reconciled;
            } else if let Some(text) = text {
                if let Some(index) = state.pending.iter().position(|pending| {
                    pending.identity == identity
                        && pending.text == text
                        && matches!(
                            pending.state,
                            PendingClaudeComposerPromptState::Reserved {
                                deferred_hook: None
                            } | PendingClaudeComposerPromptState::Accepted
                        )
                }) {
                    let accepted = matches!(
                        state.pending[index].state,
                        PendingClaudeComposerPromptState::Accepted
                    );
                    if accepted {
                        let pending = state
                            .pending
                            .remove(index)
                            .expect("matched Claude reconciliation exists");
                        if let Some(key) = draft.deduplication_key.clone() {
                            remember_reconciled_claude_provider_key(
                                &mut state,
                                pending.identity,
                                key,
                                Instant::now(),
                            );
                        }
                    } else {
                        state.pending[index].state = PendingClaudeComposerPromptState::Reserved {
                            deferred_hook: Some(draft.clone()),
                        };
                    }
                    decision = Decision::Reconciled;
                } else if draft.deduplication_key.as_ref().is_some_and(|key| {
                    state.pending.iter().any(|pending| {
                        pending.identity == identity
                            && pending.text == text
                            && matches!(
                                &pending.state,
                                PendingClaudeComposerPromptState::Reserved {
                                    deferred_hook: Some(deferred)
                                } if deferred.deduplication_key.as_ref() == Some(key)
                            )
                    })
                }) {
                    decision = Decision::Reconciled;
                }
            }
            (expired, decision)
        };

        for expired in expired {
            self.push_semantic_draft(expired);
        }
        if matches!(decision, Decision::Publish) {
            self.push_semantic_draft(draft);
        }
    }

    #[must_use]
    pub(crate) fn reserve_claude_composer_prompt(
        &self,
        mutation_id: &str,
        pty_session_id: &str,
        stable_session_key: &StableSessionKey,
        text: &str,
    ) -> ComposerReconciliationReservation {
        let (deferred, reservation) = {
            let mut state = self
                .inner
                .claude_composer_reconciliation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = Instant::now();
            let deferred = drain_expired_claude_reconciliations(&mut state, now);
            let identity = state
                .adapters_by_pty_session
                .get(pty_session_id)
                .filter(|identity| &identity.stable_session_key == stable_session_key)
                .cloned();
            let reservation = match identity {
                None => ComposerReconciliationReservation::NotNeeded,
                Some(_) if state.pending.len() >= MAX_CLAUDE_COMPOSER_RECONCILIATIONS => {
                    ComposerReconciliationReservation::CapacityExceeded
                }
                Some(identity) => {
                    state.pending.push_back(PendingClaudeComposerPrompt {
                        mutation_id: mutation_id.to_string(),
                        identity,
                        text: text.to_string(),
                        state: PendingClaudeComposerPromptState::Reserved {
                            deferred_hook: None,
                        },
                        expires_at: now + CLAUDE_COMPOSER_RECONCILIATION_TTL,
                    });
                    ComposerReconciliationReservation::Reserved
                }
            };
            (deferred, reservation)
        };
        for draft in deferred {
            self.push_semantic_draft(draft);
        }
        reservation
    }

    pub(crate) fn accept_claude_composer_prompt(&self, mutation_id: &str) {
        let mut state = self
            .inner
            .claude_composer_reconciliation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(index) = state
            .pending
            .iter()
            .position(|pending| pending.mutation_id == mutation_id)
        else {
            return;
        };
        let deferred = match &mut state.pending[index].state {
            PendingClaudeComposerPromptState::Reserved { deferred_hook } => deferred_hook.take(),
            PendingClaudeComposerPromptState::Accepted => return,
        };
        if let Some(deferred) = deferred {
            let pending = state
                .pending
                .remove(index)
                .expect("matched Claude reconciliation exists");
            if let Some(key) = deferred.deduplication_key {
                remember_reconciled_claude_provider_key(
                    &mut state,
                    pending.identity,
                    key,
                    Instant::now(),
                );
            }
        } else {
            state.pending[index].state = PendingClaudeComposerPromptState::Accepted;
        }
    }

    pub(crate) fn cancel_claude_composer_prompt(&self, mutation_id: &str) {
        let deferred = {
            let mut state = self
                .inner
                .claude_composer_reconciliation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state
                .pending
                .iter()
                .position(|pending| pending.mutation_id == mutation_id)
                .and_then(|index| state.pending.remove(index))
                .and_then(deferred_claude_hook)
        };
        if let Some(draft) = deferred {
            self.push_semantic_draft(draft);
        }
    }

    pub fn push_codex_adapter_registered(&self, identity: CodexSemanticIdentity) {
        let deferred = {
            let mut state = self
                .inner
                .codex_composer_reconciliation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let deferred = drain_expired_codex_reconciliations(&mut state, Instant::now());
            state
                .adapters_by_pty_session
                .insert(identity.pty_session_id.clone(), identity);
            deferred
        };
        for draft in deferred {
            self.push_semantic_draft(draft);
        }
    }

    pub fn push_codex_adapter_removed(&self, identity: &CodexSemanticIdentity) {
        let deferred = {
            let mut state = self
                .inner
                .codex_composer_reconciliation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let deferred = drain_expired_codex_reconciliations(&mut state, Instant::now());
            if state.adapters_by_pty_session.get(&identity.pty_session_id) == Some(identity) {
                state
                    .adapters_by_pty_session
                    .remove(&identity.pty_session_id);
            }
            // Keep generation-scoped reservations and retry tombstones across
            // bridge exit/replacement. PTY acceptance and provider delivery
            // are independently asynchronous.
            deferred
        };
        for draft in deferred {
            self.push_semantic_draft(draft);
        }
    }

    pub fn push_codex_semantic_draft(
        &self,
        identity: CodexSemanticIdentity,
        draft: SemanticEventDraft,
    ) {
        enum Decision {
            Publish,
            Reconciled,
        }

        let (expired, decision) = {
            let mut state = self
                .inner
                .codex_composer_reconciliation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let expired = drain_expired_codex_reconciliations(&mut state, Instant::now());
            let provider_key_reconciled = draft.deduplication_key.as_ref().is_some_and(|key| {
                state
                    .reconciled_provider_keys
                    .iter()
                    .any(|entry| entry.identity == identity && entry.key == *key)
            });
            let text = match &draft.kind {
                presentation::SemanticEventKind::UserMessage { text } => Some(text.as_str()),
                _ => None,
            };
            let mut decision = Decision::Publish;
            if provider_key_reconciled {
                decision = Decision::Reconciled;
            } else if let Some(text) = text {
                if let Some(index) = state.pending.iter().position(|pending| {
                    pending.identity == identity
                        && pending.text == text
                        && matches!(
                            pending.state,
                            PendingCodexComposerPromptState::Reserved {
                                deferred_provider: None
                            } | PendingCodexComposerPromptState::Accepted
                        )
                }) {
                    let accepted = matches!(
                        state.pending[index].state,
                        PendingCodexComposerPromptState::Accepted
                    );
                    if accepted {
                        let pending = state
                            .pending
                            .remove(index)
                            .expect("matched Codex reconciliation exists");
                        if let Some(key) = draft.deduplication_key.clone() {
                            remember_reconciled_codex_provider_key(
                                &mut state,
                                pending.identity,
                                key,
                                Instant::now(),
                            );
                        }
                    } else {
                        state.pending[index].state = PendingCodexComposerPromptState::Reserved {
                            deferred_provider: Some(draft.clone()),
                        };
                    }
                    decision = Decision::Reconciled;
                } else if draft.deduplication_key.as_ref().is_some_and(|key| {
                    state.pending.iter().any(|pending| {
                        pending.identity == identity
                            && pending.text == text
                            && matches!(
                                &pending.state,
                                PendingCodexComposerPromptState::Reserved {
                                    deferred_provider: Some(deferred)
                                } if deferred.deduplication_key.as_ref() == Some(key)
                            )
                    })
                }) {
                    decision = Decision::Reconciled;
                }
            }
            (expired, decision)
        };

        for expired in expired {
            self.push_semantic_draft(expired);
        }
        if matches!(decision, Decision::Publish) {
            self.push_semantic_draft(draft);
        }
    }

    #[must_use]
    pub(crate) fn reserve_codex_composer_prompt(
        &self,
        mutation_id: &str,
        pty_session_id: &str,
        stable_session_key: &StableSessionKey,
        text: &str,
    ) -> ComposerReconciliationReservation {
        let (deferred, reservation) = {
            let mut state = self
                .inner
                .codex_composer_reconciliation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = Instant::now();
            let deferred = drain_expired_codex_reconciliations(&mut state, now);
            let identity = state
                .adapters_by_pty_session
                .get(pty_session_id)
                .filter(|identity| &identity.stable_session_key == stable_session_key)
                .cloned();
            let reservation = match identity {
                None => ComposerReconciliationReservation::NotNeeded,
                Some(_) if state.pending.len() >= MAX_CODEX_COMPOSER_RECONCILIATIONS => {
                    ComposerReconciliationReservation::CapacityExceeded
                }
                Some(identity) => {
                    state.pending.push_back(PendingCodexComposerPrompt {
                        mutation_id: mutation_id.to_string(),
                        identity,
                        text: text.to_string(),
                        state: PendingCodexComposerPromptState::Reserved {
                            deferred_provider: None,
                        },
                        expires_at: now + CODEX_COMPOSER_RECONCILIATION_TTL,
                    });
                    ComposerReconciliationReservation::Reserved
                }
            };
            (deferred, reservation)
        };
        for draft in deferred {
            self.push_semantic_draft(draft);
        }
        reservation
    }

    pub(crate) fn accept_codex_composer_prompt(&self, mutation_id: &str) {
        let mut state = self
            .inner
            .codex_composer_reconciliation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(index) = state
            .pending
            .iter()
            .position(|pending| pending.mutation_id == mutation_id)
        else {
            return;
        };
        let deferred = match &mut state.pending[index].state {
            PendingCodexComposerPromptState::Reserved { deferred_provider } => {
                deferred_provider.take()
            }
            PendingCodexComposerPromptState::Accepted => return,
        };
        if let Some(deferred) = deferred {
            let pending = state
                .pending
                .remove(index)
                .expect("matched Codex reconciliation exists");
            if let Some(key) = deferred.deduplication_key {
                remember_reconciled_codex_provider_key(
                    &mut state,
                    pending.identity,
                    key,
                    Instant::now(),
                );
            }
        } else {
            state.pending[index].state = PendingCodexComposerPromptState::Accepted;
        }
    }

    pub(crate) fn cancel_codex_composer_prompt(&self, mutation_id: &str) {
        let deferred = {
            let mut state = self
                .inner
                .codex_composer_reconciliation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state
                .pending
                .iter()
                .position(|pending| pending.mutation_id == mutation_id)
                .and_then(|index| state.pending.remove(index))
                .and_then(deferred_codex_provider)
        };
        if let Some(draft) = deferred {
            self.push_semantic_draft(draft);
        }
    }

    pub fn push_semantic_adapter_health(
        &self,
        stable_session_key: StableSessionKey,
        health: SemanticAdapterHealth,
    ) {
        let changed = self.publish_semantic_change(|journals| {
            journals.set_adapter_health(&stable_session_key, health)
        });
        if changed {
            let _ = deliver_live_semantic_events(&self.inner);
        }
    }

    pub fn push_session_output_with_mode(
        &self,
        session_id: &str,
        bytes: Vec<u8>,
        mode: TerminalModeSnapshot,
        screen: Option<TerminalScreenSnapshot>,
    ) {
        self.push_session_output_inner(session_id, bytes, Some(mode), screen);
    }

    fn push_session_output_inner(
        &self,
        session_id: &str,
        bytes: Vec<u8>,
        mode: Option<TerminalModeSnapshot>,
        screen: Option<TerminalScreenSnapshot>,
    ) {
        if bytes.is_empty() {
            return;
        }
        let emitted_at_epoch_ms = now_epoch_ms();
        let tabs = self
            .inner
            .shared_state
            .read()
            .map(|state| state.open_tabs.clone())
            .unwrap_or_default();
        let runtime = self
            .inner
            .runtime_state
            .read()
            .ok()
            .and_then(|state| state.sessions.get(session_id).cloned());
        self.publish_semantic_change(|journals| {
            let runtime_changed = runtime.as_ref().is_some_and(|runtime| {
                journals.observe_runtime(runtime, &tabs, emitted_at_epoch_ms)
            });
            let mode_changed = mode.is_some_and(|mode| {
                journals.observe_native_terminal_mode(session_id, mode, emitted_at_epoch_ms)
            });
            let output_changed =
                journals.observe_output(session_id, &bytes, screen.as_ref(), emitted_at_epoch_ms);
            runtime_changed || mode_changed || output_changed
        });
        self.mark_subscribed_clients_bootstrap_pending(session_id);
        let targets = self
            .inner
            .clients
            .lock()
            .map(|clients| {
                clients
                    .iter()
                    .filter_map(|(connection_id, client)| {
                        client
                            .subscribed_session_ids
                            .contains(session_id)
                            .then(|| (*connection_id, client_delivery_target(client)))
                    })
                    .filter_map(|(connection_id, target)| {
                        target.map(|target| (connection_id, target))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (connection_id, target) in targets {
            let message = ServerMessage::SessionStream {
                event: RemoteSessionStreamEvent::Output {
                    session_id: session_id.to_string(),
                    chunk_seq: self
                        .inner
                        .next_output_chunk_seq
                        .fetch_add(1, Ordering::Relaxed),
                    emitted_at_epoch_ms,
                    bytes: bytes.clone(),
                },
            };
            if !deliver_server_message(&self.inner, connection_id, &target, message) {
                revoke_failed_delivery(&self.inner, connection_id, target);
            }
        }
    }

    pub fn push_session_runtime(&self, session_id: &str, runtime: SessionRuntimeState) {
        let mut runtime = runtime;
        if runtime.session_kind == SessionKind::Ssh
            && runtime.status == SessionStatus::Exited
            && runtime
                .exit
                .as_ref()
                .is_none_or(|exit| !exit.closed_by_user)
        {
            runtime.status = SessionStatus::Failed;
        }
        let visibility_guard = self
            .inner
            .web_control_operation_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tabs = self
            .inner
            .shared_state
            .read()
            .map(|state| state.open_tabs.clone())
            .unwrap_or_default();
        let mut push_transition = None;
        self.publish_semantic_change(|journals| {
            let previous_status = journals.status_for_session(session_id);
            let previous_attention = journals
                .stable_key_for_session(session_id)
                .and_then(|key| journals.metadata(&key))
                .map(|metadata| metadata.attention);
            let changed = journals.observe_runtime(&runtime, &tabs, now_epoch_ms());
            let stable_key = journals.stable_key_for_session(session_id);
            let current_attention = stable_key
                .as_ref()
                .and_then(|key| journals.metadata(key))
                .map(|metadata| metadata.attention);
            let action = match runtime.session_kind {
                SessionKind::Server
                    if previous_status.is_some_and(SessionStatus::is_live)
                        && matches!(
                            runtime.status,
                            SessionStatus::Crashed | SessionStatus::Failed
                        ) =>
                {
                    Some(web::push::PushAttentionKind::ServerCrashed)
                }
                SessionKind::Ssh
                    if previous_status.is_some_and(SessionStatus::is_live)
                        && !runtime.status.is_live()
                        && runtime
                            .exit
                            .as_ref()
                            .is_none_or(|exit| !exit.closed_by_user) =>
                {
                    Some(web::push::PushAttentionKind::SshDisconnected)
                }
                SessionKind::Claude | SessionKind::Codex
                    if previous_status.is_some()
                        && previous_attention != Some(SemanticAttention::Unread)
                        && current_attention == Some(SemanticAttention::Unread) =>
                {
                    Some(web::push::PushAttentionKind::Completed)
                }
                _ => None,
            };
            if let (Some(stable_key), Some(action)) = (stable_key, action) {
                push_transition = Some((stable_key, action));
            }
            changed
        });
        if let Some((stable_key, action)) = push_transition {
            self.enqueue_push_attention(Some(session_id), &stable_key, action);
        }
        drop(visibility_guard);
        self.mark_subscribed_clients_bootstrap_pending(session_id);
        let targets = self
            .inner
            .clients
            .lock()
            .map(|mut clients| {
                clients
                    .iter_mut()
                    .filter_map(|(connection_id, client)| {
                        if !client.subscribed_session_ids.contains(session_id) {
                            return None;
                        }
                        if !runtime.status.is_live() {
                            client.bootstrapped_session_ids.remove(session_id);
                            client.bootstrap_pending_session_ids.remove(session_id);
                        }
                        client_delivery_target(client).map(|target| (*connection_id, target))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (connection_id, target) in targets {
            let event = if runtime.status.is_live() {
                RemoteSessionStreamEvent::RuntimePatch {
                    session_id: session_id.to_string(),
                    runtime: runtime.clone(),
                }
            } else {
                RemoteSessionStreamEvent::Closed {
                    session_id: session_id.to_string(),
                    runtime: runtime.clone(),
                }
            };
            if !deliver_server_message(
                &self.inner,
                connection_id,
                &target,
                ServerMessage::SessionStream { event },
            ) {
                revoke_failed_delivery(&self.inner, connection_id, target);
            }
        }
    }

    fn enqueue_push_attention(
        &self,
        session_id: Option<&str>,
        stable_session_key: &StableSessionKey,
        action: web::push::PushAttentionKind,
    ) {
        let sender = self
            .inner
            .web_push_sender
            .read()
            .ok()
            .and_then(|sender| sender.as_ref().map(|registered| registered.sender.clone()));
        let Some(sender) = sender else {
            return;
        };

        let focused_sessions = self
            .inner
            .clients
            .lock()
            .map(|clients| {
                clients
                    .values()
                    .filter_map(|client| {
                        client
                            .focused_session_id
                            .as_ref()
                            .map(|focused| (client.client_id.clone(), focused.clone()))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let focused_clients = if let Some(session_id) = session_id {
            focused_sessions
                .iter()
                .filter(|(_, focused)| focused == session_id)
                .map(|(client_id, _)| client_id.clone())
                .collect::<Vec<_>>()
        } else {
            self.inner
                .semantic_journals
                .lock()
                .map(|journals| {
                    focused_sessions
                        .iter()
                        .filter(|(_, focused)| {
                            journals.stable_key_for_session(focused).as_ref()
                                == Some(stable_session_key)
                        })
                        .map(|(client_id, _)| client_id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let push_config = self
            .inner
            .config
            .read()
            .map(|config| config.web.push.clone())
            .unwrap_or_default();
        let subscriptions = web::push::eligible_subscriptions(&push_config, &focused_clients);
        if subscriptions.is_empty() {
            return;
        }

        let (project_label, session_label) = self.push_labels(stable_session_key);
        let badge = self
            .inner
            .semantic_journals
            .lock()
            .map(|journals| {
                journals
                    .metadata_snapshot()
                    .values()
                    .filter(|metadata| metadata.attention != SemanticAttention::None)
                    .fold(0_u64, |total, metadata| {
                        total.saturating_add(metadata.attention_count.max(1))
                    })
            })
            .unwrap_or(1)
            .min(99);
        let event_sequence = self
            .inner
            .next_push_event_id
            .fetch_add(1, Ordering::Relaxed);
        let event_id = format!("{}-{event_sequence}", now_epoch_ms());
        let payload = web::push::PushPayload::attention(
            self.inner.runtime_instance_id.clone(),
            stable_session_key,
            action,
            &project_label,
            &session_label,
            event_id,
            badge,
        );
        for subscription in subscriptions {
            let _ = sender.try_send(web::push::PushDelivery {
                config: push_config.clone(),
                subscription,
                payload: payload.clone(),
            });
        }
    }

    fn push_labels(&self, stable_session_key: &StableSessionKey) -> (String, String) {
        let Ok(state) = self.inner.shared_state.read() else {
            return ("Project".to_string(), "Session".to_string());
        };
        if let Some(command_id) = stable_session_key.as_str().strip_prefix("server:") {
            if let Some(found) = state.find_command(command_id) {
                return (found.project.name.clone(), found.command.label.clone());
            }
        }
        if let Some(tab_id) = stable_session_key.as_str().strip_prefix("tab:") {
            if let Some(tab) = state.open_tabs.iter().find(|tab| tab.id == tab_id) {
                let project = state
                    .find_project(&tab.project_id)
                    .map(|project| project.name.clone())
                    .unwrap_or_else(|| "Project".to_string());
                let fallback = match tab.tab_type {
                    TabType::Claude => "Claude",
                    TabType::Codex => "Codex",
                    TabType::Ssh => "SSH",
                    TabType::Server => "Server",
                };
                let session = tab
                    .label
                    .clone()
                    .filter(|label| !label.trim().is_empty())
                    .unwrap_or_else(|| fallback.to_string());
                return (project, session);
            }
        }
        ("Project".to_string(), "Session".to_string())
    }

    fn publish_semantic_change(
        &self,
        mutation: impl FnOnce(&mut SemanticJournalStore) -> bool,
    ) -> bool {
        let publication_guard = match self.inner.semantic_publication_lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let guard = poisoned.into_inner();
                self.inner.semantic_publication_lock.clear_poison();
                guard
            }
        };
        let previous_generation = self
            .inner
            .semantic_publication_generation
            .fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(previous_generation % 2, 0);
        let epoch = SemanticPublicationEpoch {
            generation: &self.inner.semantic_publication_generation,
        };

        let mut journals = match self.inner.semantic_journals.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let guard = poisoned.into_inner();
                self.inner.semantic_journals.clear_poison();
                guard
            }
        };
        let mutation_result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| mutation(&mut journals)));
        // The panic was caught while the journal guard was still alive, so a
        // normal drop here keeps the store usable by later publications.
        drop(journals);

        let changed = match mutation_result {
            Ok(changed) => changed,
            Err(payload) => {
                {
                    let _snapshot_guard = self
                        .inner
                        .snapshot_state_lock
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    self.inner.snapshot_revision.fetch_add(1, Ordering::Relaxed);
                    notify_broadcaster(&self.inner);
                }
                // Keep the generation odd until the conservative revision is
                // visible, and release both guards normally before unwinding.
                drop(epoch);
                drop(publication_guard);
                std::panic::resume_unwind(payload);
            }
        };
        if changed {
            #[cfg(test)]
            self.run_semantic_publication_test_hook();
            let _snapshot_guard = self
                .inner
                .snapshot_state_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.inner.snapshot_revision.fetch_add(1, Ordering::Relaxed);
            notify_broadcaster(&self.inner);
        }
        drop(epoch);
        drop(publication_guard);
        changed
    }

    fn acknowledge_semantic_attention(&self, stable_session_key: &StableSessionKey) {
        self.publish_semantic_change(|journals| {
            if journals
                .metadata(stable_session_key)
                .is_none_or(|metadata| metadata.attention == SemanticAttention::NeedsInput)
            {
                return false;
            }
            journals.set_attention(stable_session_key, SemanticAttention::None, 0)
        });
    }

    #[cfg(test)]
    fn run_semantic_publication_test_hook(&self) {
        let hook = self
            .inner
            .semantic_publication_test_hook
            .read()
            .ok()
            .and_then(|hook| hook.clone());
        if let Some(hook) = hook {
            hook();
        }
    }

    pub fn push_session_removed(&self, session_id: &str) {
        self.publish_semantic_change(|journals| {
            journals.remove_session_binding(session_id).is_some()
        });
        let targets = self
            .inner
            .clients
            .lock()
            .map(|mut clients| {
                clients
                    .iter_mut()
                    .filter_map(|(connection_id, client)| {
                        if !client.subscribed_session_ids.contains(session_id) {
                            return None;
                        }
                        client.bootstrapped_session_ids.remove(session_id);
                        client.bootstrap_pending_session_ids.remove(session_id);
                        client_delivery_target(client).map(|target| (*connection_id, target))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (connection_id, target) in targets {
            if !deliver_server_message(
                &self.inner,
                connection_id,
                &target,
                ServerMessage::SessionStream {
                    event: RemoteSessionStreamEvent::Removed {
                        session_id: session_id.to_string(),
                    },
                },
            ) {
                revoke_failed_delivery(&self.inner, connection_id, target);
            }
        }
    }

    pub fn drain_requests(&self) -> Vec<PendingRemoteRequest> {
        let Ok(mut requests) = self.inner.pending_requests.lock() else {
            return Vec::new();
        };
        requests.drain(..).collect()
    }

    pub fn has_pending_requests(&self) -> bool {
        self.inner
            .pending_requests
            .lock()
            .map(|requests| !requests.is_empty())
            .unwrap_or(false)
    }

    pub fn status(&self) -> RemoteHostStatus {
        let (enabled, web_enabled, bind_address, port, pairing_token) = self
            .inner
            .config
            .read()
            .map(|config| {
                (
                    config.enabled,
                    config.web.enabled,
                    config.bind_address.clone(),
                    config.port,
                    config.pairing_token.clone(),
                )
            })
            .unwrap_or_default();
        let (connected_clients, connected_native_clients, connected_web_clients) = self
            .inner
            .clients
            .lock()
            .map(|clients| {
                let connected_clients = clients.len();
                let connected_web_clients = clients
                    .values()
                    .filter(|client| client.client_id.starts_with("web-"))
                    .count();
                let connected_native_clients =
                    connected_clients.saturating_sub(connected_web_clients);
                (
                    connected_clients,
                    connected_native_clients,
                    connected_web_clients,
                )
            })
            .unwrap_or((0, 0, 0));
        let controller_client_id = self
            .inner
            .controller_client_id
            .read()
            .map(|slot| slot.clone())
            .unwrap_or_default();
        let listening = self.inner.listener_running.load(Ordering::Relaxed);
        let listener_error = self
            .inner
            .listener_error
            .read()
            .map(|slot| slot.clone())
            .unwrap_or(None);
        let web_listener_error = self
            .inner
            .web_listener_error
            .read()
            .map(|slot| slot.clone())
            .unwrap_or(None);
        let last_connection_note = self
            .inner
            .last_connection_note
            .read()
            .map(|slot| slot.clone())
            .unwrap_or(None);
        let last_connection_is_error = self.inner.last_connection_is_error.load(Ordering::Relaxed);
        let latency = self
            .inner
            .latency
            .read()
            .map(|stats| stats.clone())
            .unwrap_or_default();
        let connect_startup_error = self
            .inner
            .connect_startup_error
            .read()
            .map(|slot| slot.clone())
            .unwrap_or(None);
        let connect_listener_bound = self.inner.connect_listener_bound.load(Ordering::Acquire);
        let connect_encryption_required = self
            .inner
            .connect_encryption_required
            .load(Ordering::Acquire);
        RemoteHostStatus {
            enabled,
            web_enabled,
            bind_address,
            port,
            pairing_token,
            connected_clients,
            connected_native_clients,
            connected_web_clients,
            controller_client_id,
            listening,
            listener_error,
            web_listener_error,
            last_connection_note,
            last_connection_is_error,
            latency,
            connect_startup_error,
            connect_listener_bound,
            connect_encryption_required,
        }
    }

    pub fn revoke_paired_client(&self, client_id: &str) -> bool {
        let removed = match mutate_host_config_if(
            &self.inner,
            |config| {
                config
                    .paired_clients
                    .iter()
                    .any(|client| client.client_id == client_id)
            },
            |config| {
                config
                    .paired_clients
                    .retain(|client| client.client_id != client_id);
            },
        ) {
            Ok(Some(())) => true,
            Ok(None) | Err(_) => false,
        };

        if removed {
            if let Ok(mut clients) = self.inner.clients.lock() {
                let connection_ids: Vec<u64> = clients
                    .iter()
                    .filter_map(|(connection_id, client)| {
                        (client.client_id == client_id).then_some(*connection_id)
                    })
                    .collect();
                for connection_id in connection_ids {
                    if let Some(client) = clients.remove(&connection_id) {
                        if let Some(sender) = client.sender.as_ref() {
                            let _ = sender.send(ServerMessage::Disconnected {
                                message: "This host revoked the saved client token.".to_string(),
                            });
                        }
                    }
                }
            }
        }

        if removed {
            if let Ok(mut controller) = self.inner.controller_client_id.write() {
                if controller.as_deref() == Some(client_id) {
                    *controller = None;
                }
            }
        }

        removed
    }

    pub fn revoke_paired_web_client(&self, client_id: &str) -> bool {
        let _transaction = match self.inner.host_config_tx.lock() {
            Ok(transaction) => transaction,
            Err(_) => return false,
        };
        let attempt_id = self
            .inner
            .next_host_config_attempt_id
            .fetch_add(1, Ordering::Relaxed);
        let staged = {
            let _operation = self
                .inner
                .web_control_operation_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match stage_host_config_mutation(&self.inner, |config| {
                let before = config.web.paired_clients.len();
                config
                    .web
                    .paired_clients
                    .retain(|client| client.client_id != client_id);
                config.web.connect_peer_keys.remove(client_id);
                config.web.activity_log.retain(|event| {
                    !(event.source == RemoteAccessSource::Browser && event.client_id == client_id)
                });
                config.web.push.remove_client(client_id);
                config.web.paired_clients.len() != before
            }) {
                Ok(staged) => staged,
                Err(_) => return false,
            }
        };
        if persist_host_config_snapshot(&staged.candidate).is_err() {
            return false;
        }
        let commit_result = {
            let _operation = self
                .inner
                .web_control_operation_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            commit_staged_host_config_mutation(&self.inner, &staged)
        };
        if let Err(error) = commit_result {
            let error = match compensate_rejected_host_config_admission(&staged, attempt_id) {
                Ok(()) => HostConfigAdmissionError::Persistence(error),
                Err(error) => error,
            };
            bump_host_config_revision(&self.inner);
            set_last_connection_note(
                &self.inner,
                format!("Browser revoke durability failed: {error}"),
                true,
            );
            return false;
        };
        let removed = staged.result;
        drop(_transaction);

        let connections = self
            .inner
            .clients
            .lock()
            .map(|clients| {
                clients
                    .iter()
                    .filter_map(|(connection_id, client)| {
                        (client.client_id == client_id).then(|| {
                            Some((
                                *connection_id,
                                client.client_id.clone(),
                                client.web_tombstone.clone()?,
                            ))
                        })?
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (connection_id, registered_client_id, tombstone) in connections {
            web::bridge::revoke_web_connection_locked(
                &self.inner,
                connection_id,
                &registered_client_id,
                &tombstone,
                Some("This browser invite was revoked. Pair again to reconnect.".to_string()),
            );
        }
        if removed {
            web::bridge::broadcast_writer_lease_state_locked(&self.inner, now_epoch_ms());
        }

        removed
    }

    pub fn reset_browser_access(&self) -> bool {
        let _transaction = match self.inner.host_config_tx.lock() {
            Ok(transaction) => transaction,
            Err(_) => return false,
        };
        let attempt_id = self
            .inner
            .next_host_config_attempt_id
            .fetch_add(1, Ordering::Relaxed);
        let staged = {
            let _operation = self
                .inner
                .web_control_operation_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match stage_host_config_mutation(&self.inner, |config| {
                let removed_ids = config
                    .web
                    .paired_clients
                    .iter()
                    .map(|client| client.client_id.clone())
                    .collect::<Vec<_>>();
                config.web.paired_clients.clear();
                config.web.connect_peer_keys.clear();
                config.web.push.enabled_client_ids.clear();
                config.web.push.subscriptions.clear();
                config
                    .web
                    .activity_log
                    .retain(|event| event.source != RemoteAccessSource::Browser);
                config.web.pairing_token = web::generate_web_pairing_token();
                config.web.cookie_secret_hex = web::generate_cookie_secret_hex();
                removed_ids
            }) {
                Ok(staged) => staged,
                Err(_) => return false,
            }
        };
        if persist_host_config_snapshot(&staged.candidate).is_err() {
            return false;
        }
        let commit_result = {
            let _operation = self
                .inner
                .web_control_operation_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            commit_staged_host_config_mutation(&self.inner, &staged)
        };
        if let Err(error) = commit_result {
            let error = match compensate_rejected_host_config_admission(&staged, attempt_id) {
                Ok(()) => HostConfigAdmissionError::Persistence(error),
                Err(error) => error,
            };
            bump_host_config_revision(&self.inner);
            set_last_connection_note(
                &self.inner,
                format!("Browser reset durability failed: {error}"),
                true,
            );
            return false;
        };
        let removed_client_ids = staged.result;
        drop(_transaction);
        let removed_client_ids: HashSet<String> = removed_client_ids.into_iter().collect();
        let connections = self
            .inner
            .clients
            .lock()
            .map(|clients| {
                clients
                    .iter()
                    .filter_map(|(connection_id, client)| {
                        (client.client_id.starts_with("web-")
                            || removed_client_ids.contains(client.client_id.as_str()))
                        .then(|| {
                            Some((
                                *connection_id,
                                client.client_id.clone(),
                                client.web_tombstone.clone()?,
                            ))
                        })?
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (connection_id, registered_client_id, tombstone) in connections {
            web::bridge::revoke_web_connection_locked(
                &self.inner,
                connection_id,
                &registered_client_id,
                &tombstone,
                Some("Browser access was reset. Pair again to reconnect.".to_string()),
            );
        }
        web::bridge::broadcast_writer_lease_state_locked(&self.inner, now_epoch_ms());

        true
    }

    pub fn local_has_control(&self) -> bool {
        self.inner
            .controller_client_id
            .read()
            .map(|slot| slot.is_none())
            .unwrap_or(true)
    }

    pub fn take_local_control(&self) {
        set_native_controller(&self.inner, None);
    }

    fn bump_config_revision(&self) {
        bump_host_config_revision(&self.inner);
    }

    fn restart_threads(&self) {
        let (generation, listener_worker, broadcaster_worker, web_listener, test_hook) = {
            let _lifecycle_guard = self
                .inner
                .lifecycle_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.inner.stop_flag.load(Ordering::Acquire) {
                return;
            }
            let generation = self
                .inner
                .native_runtime_generation
                .fetch_add(1, Ordering::SeqCst)
                .wrapping_add(1);
            self.inner.listener_running.store(false, Ordering::Release);
            if let Ok(mut error) = self.inner.listener_error.write() {
                *error = None;
            }
            if let Ok(mut note) = self.inner.last_connection_note.write() {
                *note = None;
            }
            self.inner
                .last_connection_is_error
                .store(false, Ordering::Release);
            wake_native_listener(&self.inner);
            notify_broadcaster(&self.inner);
            if let Ok(mut error) = self.inner.web_listener_error.write() {
                *error = None;
            }
            #[cfg(test)]
            let test_hook = self
                .inner
                .lifecycle_lock_acquired_test_hook
                .read()
                .ok()
                .and_then(|slot| slot.clone());
            #[cfg(not(test))]
            let test_hook: Option<Arc<dyn Fn() + Send + Sync>> = None;

            (
                generation,
                self.inner
                    .listener_thread
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take(),
                self.inner
                    .broadcaster_thread
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take(),
                self.inner
                    .web_listener
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take(),
                test_hook,
            )
        };
        cancel_native_connection_workers_before_generation(&self.inner, generation);
        if let Some(hook) = test_hook {
            hook();
        }

        // Stop accepting browser connections first. Tokio shutdown may cancel
        // WebSocket tasks before their async unregister tail runs, so drain
        // any records left behind immediately afterwards. This ordering also
        // closes the narrow race where a new browser could register between a
        // pre-shutdown drain and runtime teardown.
        drain_web_clients_for_restart(&self.inner);
        let deadline = Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT;
        if let Some(handle) = web_listener {
            settle_web_listener(&self.inner, handle, "remote-web-restart", deadline);
        }
        drain_web_clients_for_restart(&self.inner);
        if let Some(worker) = listener_worker {
            settle_remote_worker(&self.inner, worker, deadline);
        }
        if let Some(worker) = broadcaster_worker {
            settle_remote_worker(&self.inner, worker, deadline);
        }
        join_native_connection_workers_before_generation(&self.inner, generation, deadline);

        let config = self
            .inner
            .config
            .read()
            .map(|slot| slot.clone())
            .unwrap_or_default();

        let web_only = self.inner.web_only_execution;
        let (native_lease, web_lease) = if web_only {
            let web = if config.web.enabled {
                match acquire_listener_lease(&self.inner, config.web.port, generation) {
                    Ok(lease) => Some(lease),
                    Err(error) => {
                        if let Ok(mut slot) = self.inner.web_listener_error.write() {
                            *slot = Some(format!("Web listener reservation failed: {error}"));
                        }
                        return;
                    }
                }
            } else {
                None
            };
            (None, web)
        } else {
            match acquire_config_listener_leases(&self.inner, generation, &config) {
                Ok(leases) => leases,
                Err(error) => {
                    if config.enabled {
                        if let Ok(mut slot) = self.inner.listener_error.write() {
                            *slot = Some(format!("Listener reservation failed: {error}"));
                        }
                    }
                    if config.web.enabled {
                        if let Ok(mut slot) = self.inner.web_listener_error.write() {
                            *slot = Some(format!("Web listener reservation failed: {error}"));
                        }
                    }
                    return;
                }
            }
        };

        let mut new_listener_worker = if config.enabled && !web_only {
            let listener_inner = Arc::downgrade(&self.inner);
            match RemoteWorker::try_spawn("remote-native-listener", None, move || {
                run_listener(listener_inner, generation, native_lease);
            }) {
                Ok(worker) => Some(worker),
                Err(error) => {
                    if let Ok(mut slot) = self.inner.listener_error.write() {
                        *slot = Some(format!("Listener worker unavailable: {error}"));
                    }
                    None
                }
            }
        } else {
            drop(native_lease);
            None
        };
        let mut new_broadcaster_worker = if !web_only && (config.enabled || config.web.enabled) {
            let broadcaster_inner = Arc::downgrade(&self.inner);
            let broadcaster_signal = self.inner.broadcaster_signal.clone();
            match RemoteWorker::try_spawn("remote-broadcaster", None, move || {
                run_broadcaster(broadcaster_inner, broadcaster_signal, generation);
            }) {
                Ok(worker) => Some(worker),
                Err(error) => {
                    if config.enabled {
                        if let Ok(mut slot) = self.inner.listener_error.write() {
                            *slot = Some(format!("Broadcaster worker unavailable: {error}"));
                        }
                    }
                    if config.web.enabled {
                        if let Ok(mut slot) = self.inner.web_listener_error.write() {
                            *slot = Some(format!("Broadcaster worker unavailable: {error}"));
                        }
                    }
                    None
                }
            }
        } else {
            None
        };
        let installed = {
            let _lifecycle_guard = self
                .inner
                .lifecycle_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.inner.stop_flag.load(Ordering::Acquire)
                || self.inner.native_runtime_generation.load(Ordering::Acquire) != generation
                || (config.enabled && !web_only && new_listener_worker.is_none())
                || (!web_only
                    && (config.enabled || config.web.enabled)
                    && new_broadcaster_worker.is_none())
            {
                false
            } else {
                if let Some(worker) = new_listener_worker.take() {
                    *self
                        .inner
                        .listener_thread
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(worker);
                }

                // The broadcaster drives snapshot/delta fan-out to every connected
                // client, regardless of transport. Run it whenever any listener is
                // enabled — the native TCP one, the browser web one, or both.
                if let Some(worker) = new_broadcaster_worker.take() {
                    *self
                        .inner
                        .broadcaster_thread
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(worker);
                }
                true
            }
        };
        if !installed {
            let deadline = Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT;
            if let Some(worker) = new_listener_worker {
                settle_remote_worker(&self.inner, worker, deadline);
            }
            if let Some(worker) = new_broadcaster_worker {
                settle_remote_worker(&self.inner, worker, deadline);
            }
            return;
        }

        // Web listener runs independently of the native TCP listener: users
        // can enable just the web UI if they only care about browser access,
        // or vice versa.
        if config.web.enabled {
            match WebListenerHandle::start(
                self.inner.clone(),
                config.web.clone(),
                web_lease.expect("enabled web listener must have a lease"),
            ) {
                Ok(handle) => {
                    let mut stale_handle = Some(handle);
                    {
                        let _lifecycle_guard = self
                            .inner
                            .lifecycle_lock
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if !self.inner.stop_flag.load(Ordering::Acquire)
                            && self.inner.native_runtime_generation.load(Ordering::Acquire)
                                == generation
                        {
                            if let Some(handle) = stale_handle.as_ref() {
                                handle.publish_push_sender();
                            }
                            *self
                                .inner
                                .web_listener
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                stale_handle.take();
                            match crate::connect::ConnectProductionStartup::prepare_direct(
                                crate::connect::DirectBindPolicy::loopback(),
                            ) {
                                Ok(startup) => {
                                    let _ = startup.session();
                                    self.inner
                                        .connect_encryption_required
                                        .store(true, Ordering::Release);
                                    self.inner
                                        .connect_listener_bound
                                        .store(true, Ordering::Release);
                                    surface_connect_startup(&self.inner, None, false);
                                }
                                Err(error) => {
                                    self.inner
                                        .connect_listener_bound
                                        .store(false, Ordering::Release);
                                    self.inner
                                        .connect_encryption_required
                                        .store(true, Ordering::Release);
                                    let is_error = !error.is_unenrolled_identity();
                                    surface_connect_startup(
                                        &self.inner,
                                        Some(error.to_string()),
                                        is_error,
                                    );
                                }
                            }
                        }
                    }
                    if let Some(handle) = stale_handle.take() {
                        settle_web_listener(
                            &self.inner,
                            handle,
                            "remote-stale-web-listener",
                            Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT,
                        );
                    }
                }
                Err(error) => {
                    let _lifecycle_guard = self
                        .inner
                        .lifecycle_lock
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if self.inner.native_runtime_generation.load(Ordering::Acquire) == generation {
                        if let Ok(mut error_slot) = self.inner.web_listener_error.write() {
                            *error_slot = Some(error.to_string());
                        }
                        self.inner
                            .connect_listener_bound
                            .store(false, Ordering::Release);
                        surface_connect_startup(
                            &self.inner,
                            Some(format!("web listener bind failed: {error}")),
                            true,
                        );
                        #[cfg(test)]
                        notify_native_lifecycle(
                            &self.inner,
                            NativeLifecycleTestEvent::WebListenerBindFailed,
                        );
                    }
                }
            }
        }
    }
}

pub(crate) fn acknowledge_browser_attention(
    inner: &Arc<RemoteHostInner>,
    stable_session_key: &StableSessionKey,
) {
    RemoteHostService::borrowed(inner.clone()).acknowledge_semantic_attention(stable_session_key);
}

impl RemoteClientHandle {
    pub fn connect(
        address: &str,
        port: u16,
        client_label: &str,
        auth: ClientAuth,
        expected_fingerprint: Option<&str>,
    ) -> Result<RemoteClientConnectResult, String> {
        let transport::TlsConnectResult {
            mut stream,
            certificate_fingerprint,
            handshake_deadline,
        } = transport::connect_tls(address, port, expected_fingerprint)?;
        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_label: client_label.to_string(),
            auth,
        };
        let _ = stream.sock.set_write_timeout(Some(
            handshake_deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(5)),
        ));
        write_message_until_deadline(&mut stream, &hello, handshake_deadline)
            .map_err(|error| format_handshake_stage_error(address, port, "write", &error))?;
        let response: ServerMessage = read_message_until_deadline(&mut stream, handshake_deadline)
            .map_err(|error| format_handshake_stage_error(address, port, "read", &error))?;
        let (server_id, client_id, client_token, controller_client_id, you_have_control, snapshot) =
            match response {
                ServerMessage::HelloOk {
                    protocol_version,
                    server_id,
                    certificate_fingerprint: host_fingerprint,
                    client_id,
                    client_token,
                    controller_client_id,
                    you_have_control,
                    snapshot,
                } => {
                    if protocol_version != PROTOCOL_VERSION {
                        return Err(format!(
                            "Protocol mismatch. Host uses {protocol_version}, app uses {}.",
                            PROTOCOL_VERSION
                        ));
                    }
                    if host_fingerprint != certificate_fingerprint {
                        return Err(
                            "Remote TLS fingerprint did not match the negotiated host identity."
                                .to_string(),
                        );
                    }
                    (
                        server_id,
                        client_id,
                        client_token,
                        controller_client_id,
                        you_have_control,
                        snapshot,
                    )
                }
                ServerMessage::HelloErr { message } => return Err(message),
                other => return Err(format!("Unexpected handshake response: {other:?}")),
            };

        let (tx, rx) = mpsc::channel::<ClientMessage>();
        let initial_subscriptions = session_ids_for_open_tabs(&snapshot.app_state)
            .into_iter()
            .collect::<Vec<_>>();
        let inner = Arc::new(RemoteClientInner {
            pending: Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(1),
            latest_snapshot: RwLock::new(Some(snapshot.clone())),
            session_replicas: RwLock::new(HashMap::new()),
            disconnected_message: RwLock::new(None),
            snapshot_revision: AtomicU64::new(1),
            session_stream_revision: AtomicU64::new(1),
            latency: RwLock::new(RemoteLatencyStats::default()),
            pending_paint_received_at_epoch_ms: AtomicU64::new(0),
            pending_notification_count: AtomicU64::new(0),
            client_id: client_id.clone(),
            client_token: client_token.clone(),
            server_id: server_id.clone(),
            certificate_fingerprint: certificate_fingerprint.clone(),
            address: address.to_string(),
            port,
            #[cfg(test)]
            reader_exit_test_hook: RwLock::new(None),
        });

        let socket_wakeup = stream.sock.try_clone().ok();
        let reader_inner = inner.clone();
        let reader = match RemoteWorker::try_spawn("remote-client-reader", None, move || {
            run_client_connection(stream, rx, reader_inner)
        }) {
            Ok(reader) => reader,
            Err(error) => {
                return Err(format!("Remote client reader could not start: {error}"));
            }
        };
        if !initial_subscriptions.is_empty() {
            let _ = tx.send(ClientMessage::SubscribeSessions {
                session_ids: initial_subscriptions,
            });
        }
        let connection = Arc::new(RemoteClientConnectionOwner {
            outgoing: tx,
            socket_wakeup: Mutex::new(socket_wakeup),
            reader: Mutex::new(Some(reader)),
            inner: Arc::downgrade(&inner),
        });

        Ok(RemoteClientConnectResult {
            client: Self { inner, connection },
            server_id,
            certificate_fingerprint,
            client_id,
            client_token,
            controller_client_id,
            you_have_control,
            snapshot,
        })
    }

    pub fn set_focused_session(&self, session_id: Option<String>) {
        let _ = self
            .connection
            .outgoing
            .send(ClientMessage::SetFocusedSession { session_id });
    }

    pub fn subscribe_sessions(&self, session_ids: Vec<String>) {
        if session_ids.is_empty() {
            return;
        }
        let _ = self
            .connection
            .outgoing
            .send(ClientMessage::SubscribeSessions { session_ids });
    }

    pub fn unsubscribe_sessions(&self, session_ids: Vec<String>) {
        if session_ids.is_empty() {
            return;
        }
        let _ = self
            .connection
            .outgoing
            .send(ClientMessage::UnsubscribeSessions { session_ids });
    }

    pub fn send_terminal_input(&self, input: RemoteTerminalInput) {
        let _ = self.connection.outgoing.send(ClientMessage::TerminalInput {
            input,
            enqueued_at_epoch_ms: now_epoch_ms(),
        });
    }

    pub fn send_terminal_resize(&self, session_id: String, dimensions: SessionDimensions) {
        let _ = self.connection.outgoing.send(ClientMessage::ResizeSession {
            session_id,
            dimensions,
        });
    }

    pub fn send_action(&self, action: RemoteAction) {
        let _ = self
            .connection
            .outgoing
            .send(ClientMessage::Action { action });
    }

    pub fn take_control(&self) {
        if let Ok(mut latest) = self.inner.latest_snapshot.write() {
            if let Some(snapshot) = latest.as_mut() {
                snapshot.controller_client_id = Some(self.inner.client_id.clone());
                snapshot.you_have_control = true;
            }
        }
        self.inner.snapshot_revision.fetch_add(1, Ordering::Relaxed);
        let _ = self.connection.outgoing.send(ClientMessage::TakeControl);
    }

    pub fn release_control(&self) {
        if let Ok(mut latest) = self.inner.latest_snapshot.write() {
            if let Some(snapshot) = latest.as_mut() {
                if snapshot.controller_client_id.as_deref() == Some(self.inner.client_id.as_str()) {
                    snapshot.controller_client_id = None;
                }
                snapshot.you_have_control = false;
            }
        }
        self.inner.snapshot_revision.fetch_add(1, Ordering::Relaxed);
        let _ = self.connection.outgoing.send(ClientMessage::ReleaseControl);
    }

    pub fn disconnect(&self) {
        let _ = self.connection.outgoing.send(ClientMessage::Disconnect);
    }

    pub fn request(&self, action: RemoteAction) -> Result<RemoteActionResult, String> {
        let timeout = request_timeout_for_action(&action);
        let request_id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        if let Ok(mut pending) = self.inner.pending.lock() {
            pending.insert(request_id, tx);
        }
        self.connection
            .outgoing
            .send(ClientMessage::Request { request_id, action })
            .map_err(|error| format!("Remote request failed: {error}"))?;
        rx.recv_timeout(timeout)
            .map_err(|_| "Timed out waiting for remote host.".to_string())
    }

    pub fn latest_snapshot(&self) -> Option<RemoteWorkspaceSnapshot> {
        self.inner
            .latest_snapshot
            .read()
            .ok()
            .and_then(|snapshot| snapshot.clone())
    }

    pub fn snapshot_revision(&self) -> u64 {
        self.inner.snapshot_revision.load(Ordering::Relaxed)
    }

    pub fn session_stream_revision(&self) -> u64 {
        self.inner.session_stream_revision.load(Ordering::Relaxed)
    }

    pub fn drain_pending_notifications(&self) -> u64 {
        self.inner
            .pending_notification_count
            .swap(0, Ordering::Relaxed)
    }

    pub fn session_view(&self, session_id: &str) -> Option<TerminalSessionView> {
        let view = self
            .inner
            .session_replicas
            .read()
            .ok()
            .and_then(|replicas| replicas.get(session_id).and_then(TerminalReplica::view));
        if view.is_some() {
            self.note_terminal_paint_ready();
        }
        view
    }

    pub fn apply_local_terminal_resize(&self, session_id: &str, dimensions: SessionDimensions) {
        let mut changed = false;

        if let Ok(replicas) = self.inner.session_replicas.read() {
            if let Some(replica) = replicas.get(session_id) {
                replica.apply_local_resize(dimensions);
                changed = true;
            }
        }

        if let Ok(mut latest) = self.inner.latest_snapshot.write() {
            if let Some(snapshot) = latest.as_mut() {
                if let Some(runtime) = snapshot.runtime_state.sessions.get_mut(session_id) {
                    runtime.dimensions = dimensions;
                    changed = true;
                }
                if let Some(view) = snapshot.session_views.get_mut(session_id) {
                    view.runtime.dimensions = dimensions;
                    sync_screen_snapshot_dimensions(&mut view.screen, dimensions);
                    changed = true;
                }
            }
        }

        if changed {
            self.inner.snapshot_revision.fetch_add(1, Ordering::Relaxed);
            self.inner
                .session_stream_revision
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn session_screen_text(&self, session_id: &str) -> Option<String> {
        self.inner
            .session_replicas
            .read()
            .ok()
            .and_then(|replicas| replicas.get(session_id).map(TerminalReplica::screen_text))
    }

    pub fn session_scrollback_text(&self, session_id: &str) -> Option<String> {
        self.inner
            .session_replicas
            .read()
            .ok()
            .and_then(|replicas| {
                replicas
                    .get(session_id)
                    .map(TerminalReplica::scrollback_text)
            })
    }

    pub fn latency_stats(&self) -> RemoteLatencyStats {
        self.inner
            .latency
            .read()
            .map(|stats| stats.clone())
            .unwrap_or_default()
    }

    pub fn disconnected_message(&self) -> Option<String> {
        self.inner
            .disconnected_message
            .read()
            .ok()
            .and_then(|message| message.clone())
    }

    pub fn client_id(&self) -> &str {
        &self.inner.client_id
    }

    pub fn client_token(&self) -> &str {
        &self.inner.client_token
    }

    pub fn server_id(&self) -> &str {
        &self.inner.server_id
    }

    pub fn certificate_fingerprint(&self) -> &str {
        &self.inner.certificate_fingerprint
    }

    pub fn open_port_forward(
        &self,
        requested_port: u16,
    ) -> Result<transport::ClientTlsStream, String> {
        let cancellation = Arc::new(ForwardCancellation::default());
        self.open_port_forward_with_cancellation(requested_port, &cancellation)
    }

    fn open_port_forward_with_cancellation(
        &self,
        requested_port: u16,
        cancellation: &Arc<ForwardCancellation>,
    ) -> Result<transport::ClientTlsStream, String> {
        let connect_deadline = Instant::now() + Duration::from_secs(5);
        let transport::TlsConnectResult {
            mut stream,
            certificate_fingerprint,
            handshake_deadline,
        } = transport::connect_tls_with_deadline_and_cancel(
            &self.inner.address,
            self.inner.port,
            Some(&self.inner.certificate_fingerprint),
            connect_deadline,
            || cancellation.is_cancelled(),
        )?;
        if !cancellation.register(&stream.sock) {
            let _ = stream.sock.shutdown(Shutdown::Both);
            return Err("Remote port-forward connection was cancelled.".to_string());
        }
        let result = self.finish_port_forward_handshake(
            requested_port,
            &mut stream,
            certificate_fingerprint,
            handshake_deadline,
            cancellation,
        );
        if result.is_err() {
            let _ = stream.sock.shutdown(Shutdown::Both);
        }
        result.map(|()| stream)
    }

    fn finish_port_forward_handshake(
        &self,
        requested_port: u16,
        stream: &mut transport::ClientTlsStream,
        certificate_fingerprint: String,
        handshake_deadline: Instant,
        cancellation: &ForwardCancellation,
    ) -> Result<(), String> {
        if certificate_fingerprint != self.inner.certificate_fingerprint {
            return Err(
                "Remote TLS fingerprint changed while opening the forwarded port.".to_string(),
            );
        }
        write_client_message_until_deadline_cancelled(
            stream,
            &ClientMessage::PortForwardHello {
                protocol_version: PROTOCOL_VERSION,
                server_id: self.inner.server_id.clone(),
                client_id: self.inner.client_id.clone(),
                auth_token: self.inner.client_token.clone(),
                requested_port,
            },
            handshake_deadline,
            cancellation,
        )
        .map_err(|error| format!("Port forward handshake failed: {error}"))?;
        match read_client_message_until_deadline_cancelled::<ServerMessage>(
            stream,
            handshake_deadline,
            cancellation,
        )
        .map_err(|error| format!("Port forward handshake failed: {error}"))?
        {
            ServerMessage::PortForwardOk => {
                let _ = stream.sock.set_read_timeout(None);
                let _ = stream.sock.set_write_timeout(None);
                Ok(())
            }
            ServerMessage::HelloErr { message } => Err(message),
            other => Err(format!("Unexpected port forward response: {other:?}")),
        }
    }

    #[cfg(test)]
    fn note_output_received(&self, emitted_at_epoch_ms: u64) {
        note_remote_output_received(&self.inner, emitted_at_epoch_ms);
    }

    fn note_terminal_paint_ready(&self) {
        let received_at_epoch_ms = self
            .inner
            .pending_paint_received_at_epoch_ms
            .swap(0, Ordering::Relaxed);
        if received_at_epoch_ms == 0 {
            return;
        }
        let elapsed_ms = now_epoch_ms().saturating_sub(received_at_epoch_ms);
        if let Ok(mut latency) = self.inner.latency.write() {
            latency.output_client_to_paint_ms = Some(elapsed_ms);
        }
    }
}

fn note_remote_output_received(inner: &Arc<RemoteClientInner>, emitted_at_epoch_ms: u64) {
    let now_ms = now_epoch_ms();
    if let Ok(mut latency) = inner.latency.write() {
        latency.output_host_to_client_ms = Some(now_ms.saturating_sub(emitted_at_epoch_ms));
    }
    inner
        .pending_paint_received_at_epoch_ms
        .store(now_ms, Ordering::Relaxed);
}

impl LocalPortForwardManager {
    pub fn new(client: RemoteClientHandle) -> Self {
        Self {
            inner: Arc::new(LocalPortForwardManagerInner {
                client,
                manager_handle_count: AtomicUsize::new(1),
                operation_lock: Mutex::new(()),
                entries: Mutex::new(HashMap::new()),
                worker_registry: Mutex::new(LocalPortForwardWorkerRegistry::default()),
                next_scope_id: AtomicU64::new(1),
                next_connection_id: AtomicU64::new(1),
                worker_residue_count: AtomicUsize::new(0),
                statuses: RwLock::new(HashMap::new()),
                #[cfg(test)]
                connection_handler_test_hook: RwLock::new(None),
                #[cfg(test)]
                lifecycle_test_hook: RwLock::new(None),
            }),
        }
    }

    pub fn sync_ports(&self, desired_ports: &[u16]) -> bool {
        let _operation = self
            .inner
            .operation_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let desired = desired_ports.iter().copied().collect::<HashSet<_>>();
        let now_epoch_ms = now_epoch_ms();
        let mut changed = false;

        let listener_states = self
            .inner
            .statuses
            .read()
            .map(|statuses| statuses.clone())
            .unwrap_or_default();
        let (entries_to_stop, ports_to_start, removed_ports) = {
            let Ok(mut entries) = self.inner.entries.lock() else {
                return false;
            };
            let mut entries_to_stop = Vec::new();
            let mut ports_to_start = Vec::new();
            let mut removed_ports = Vec::new();

            let existing_ports = entries.keys().copied().collect::<Vec<_>>();
            for port in existing_ports {
                if desired.contains(&port) {
                    continue;
                }
                if let Some(entry) = entries.remove(&port) {
                    entries_to_stop.push((port, entry));
                }
                removed_ports.push(port);
                changed = true;
            }

            for &port in &desired {
                let listener_active = listener_states
                    .get(&port)
                    .map(|state| state.listener_active)
                    .unwrap_or(false);
                let should_start = match entries.get(&port) {
                    Some(entry) => {
                        (!listener_active)
                            || (entry.stop.is_none() && now_epoch_ms >= entry.retry_after_epoch_ms)
                    }
                    None => true,
                };
                if !should_start {
                    continue;
                }
                if let Some(entry) = entries.remove(&port) {
                    entries_to_stop.push((port, entry));
                }
                ports_to_start.push(port);
                changed = true;
            }
            (entries_to_stop, ports_to_start, removed_ports)
        };

        let deadline = Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT;
        for (port, entry) in entries_to_stop {
            stop_local_port_forward_port(&self.inner, port, entry, deadline);
        }
        if let Ok(mut statuses) = self.inner.statuses.write() {
            for port in removed_ports {
                statuses.remove(&port);
            }
        }

        for port in ports_to_start {
            match start_local_port_forward_listener(self.inner.clone(), port) {
                Ok(entry) => {
                    install_local_port_forward_entry(&self.inner, port, entry);
                    set_port_forward_state(
                        &self.inner,
                        RemotePortForwardState {
                            port,
                            listener_active: true,
                            local_port_busy: false,
                            message: Some(format!(
                                "Forwarding http://localhost:{port} to the remote host."
                            )),
                        },
                    );
                }
                Err(error) => {
                    install_local_port_forward_entry(
                        &self.inner,
                        port,
                        LocalPortForwardEntry {
                            scope_id: None,
                            stop: None,
                            worker: None,
                            wakeup: None,
                            retry_after_epoch_ms: now_epoch_ms.saturating_add(1000),
                        },
                    );
                    let local_port_busy = error.contains("already in use");
                    set_port_forward_state(
                        &self.inner,
                        RemotePortForwardState {
                            port,
                            listener_active: false,
                            local_port_busy,
                            message: Some(error),
                        },
                    );
                }
            }
        }

        changed
    }

    pub fn shutdown(&self) {
        let _operation = self
            .inner
            .operation_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entries = self
            .inner
            .entries
            .lock()
            .map(|mut entries| entries.drain().collect::<Vec<_>>())
            .unwrap_or_default();
        for (_, entry) in &entries {
            if let Some(stop) = entry.stop.as_ref() {
                stop.store(true, Ordering::Release);
            }
        }
        let deadline = Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT;
        for (port, entry) in entries {
            stop_local_port_forward_port(&self.inner, port, entry, deadline);
        }
        settle_all_local_port_forward_connections(&self.inner, deadline);
        if let Ok(mut statuses) = self.inner.statuses.write() {
            statuses.retain(|_, state| {
                state
                    .message
                    .as_deref()
                    .is_some_and(|message| message.contains("worker residue"))
            });
        }
    }

    pub fn statuses(&self) -> HashMap<u16, RemotePortForwardState> {
        self.inner
            .statuses
            .read()
            .map(|statuses| statuses.clone())
            .unwrap_or_default()
    }

    pub fn state_for(&self, port: u16) -> Option<RemotePortForwardState> {
        self.inner
            .statuses
            .read()
            .ok()
            .and_then(|statuses| statuses.get(&port).cloned())
    }

    pub fn is_active(&self, port: u16) -> bool {
        self.state_for(port)
            .map(|state| state.listener_active)
            .unwrap_or(false)
    }
}

impl Clone for LocalPortForwardManager {
    fn clone(&self) -> Self {
        self.inner
            .manager_handle_count
            .fetch_add(1, Ordering::Relaxed);
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Drop for LocalPortForwardManager {
    fn drop(&mut self) {
        if self
            .inner
            .manager_handle_count
            .fetch_sub(1, Ordering::AcqRel)
            == 1
        {
            self.shutdown();
        }
    }
}

fn install_local_port_forward_entry(
    inner: &Arc<LocalPortForwardManagerInner>,
    port: u16,
    entry: LocalPortForwardEntry,
) {
    // Poisoning is diagnostic state, not permission to detach an admitted OS
    // worker. Recover the registry so the entry remains authoritatively owned
    // and normal shutdown can cancel and join it.
    inner
        .entries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(port, entry);
}

impl Drop for LocalPortForwardManagerInner {
    fn drop(&mut self) {
        let entries = {
            let entries = self
                .entries
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(entries)
        };
        for entry in entries.values() {
            if let Some(stop) = entry.stop.as_ref() {
                stop.store(true, Ordering::Release);
            }
        }
        let connections = {
            let registry = self
                .worker_registry
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry.active_scopes.clear();
            std::mem::take(&mut registry.connections)
        };
        for connection in connections.values() {
            connection.cancellation.cancel();
        }

        let deadline = Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT;
        for (_, mut entry) in entries {
            if let Some(wakeup) = entry.wakeup.take() {
                let _ = TcpStream::connect_timeout(&wakeup, Duration::from_millis(100));
            }
            if let Some(worker) = entry.worker.take() {
                settle_unowned_remote_worker(worker, deadline);
            }
        }
        for (_, connection) in connections {
            settle_unowned_remote_worker(connection.worker, deadline);
        }
    }
}

fn format_handshake_stage_error(address: &str, port: u16, stage: &str, error: &str) -> String {
    let trimmed = error.trim();
    let mut message = format!("Handshake failed: {trimmed}");
    if matches!(stage, "write" | "read") {
        message.push_str(&format!(
            " The host at {address}:{port} accepted the socket but closed it before the DevManager handshake finished."
        ));
        message.push_str(
            " Open Remote settings on the host and check the latest host-side error. If this is another local DevManager install, make sure it is updated to the same remote build as this app.",
        );
    }
    message
}

fn set_port_forward_state(
    inner: &Arc<LocalPortForwardManagerInner>,
    state: RemotePortForwardState,
) {
    if let Ok(mut statuses) = inner.statuses.write() {
        statuses.insert(state.port, state);
    }
}

fn stop_local_port_forward_port(
    inner: &Arc<LocalPortForwardManagerInner>,
    port: u16,
    mut entry: LocalPortForwardEntry,
    deadline: Instant,
) {
    if let Some(stop) = entry.stop.take() {
        stop.store(true, Ordering::SeqCst);
    }
    if let Some(wakeup) = entry.wakeup.take() {
        let _ = TcpStream::connect_timeout(&wakeup, Duration::from_millis(100));
    }
    let connections = close_local_port_forward_scope(inner, port, entry.scope_id.take());
    for connection in &connections {
        connection.cancellation.cancel();
    }
    #[cfg(test)]
    if let Some(hook) = inner
        .lifecycle_test_hook
        .read()
        .ok()
        .and_then(|slot| slot.clone())
    {
        hook(LocalPortForwardLifecycleTestEvent::AcceptanceClosed);
    }
    if let Some(worker) = entry.worker.take() {
        settle_local_port_forward_worker(inner, port, worker, deadline);
    }
    for connection in connections {
        settle_local_port_forward_worker(inner, port, connection.worker, deadline);
    }
}

fn defer_local_port_forward_worker(
    inner: &Arc<LocalPortForwardManagerInner>,
    port: u16,
    mut worker: RemoteWorker,
) {
    let Some(handle) = worker.handle.take() else {
        return;
    };
    inner.worker_residue_count.fetch_add(1, Ordering::AcqRel);
    set_port_forward_state(
        inner,
        RemotePortForwardState {
            port,
            listener_active: false,
            local_port_busy: false,
            message: Some(format!(
                "Local forward worker residue: {} did not stop within {} ms; DevManager still owns it until cooperative shutdown completes.",
                worker.name,
                REMOTE_WORKER_SHUTDOWN_TIMEOUT.as_millis()
            )),
        },
    );
    enqueue_deferred_remote_worker(DeferredRemoteWorker {
        name: worker.name,
        generation: 0,
        handle,
        owner: DeferredRemoteWorkerOwner::LocalPortForward {
            inner: Arc::downgrade(inner),
            port,
        },
        #[cfg(test)]
        reap_observer: None,
    });
}

fn defer_unowned_remote_worker(mut worker: RemoteWorker) {
    let Some(handle) = worker.handle.take() else {
        return;
    };
    enqueue_deferred_remote_worker(DeferredRemoteWorker {
        name: worker.name,
        generation: 0,
        handle,
        owner: DeferredRemoteWorkerOwner::Unowned,
        #[cfg(test)]
        reap_observer: None,
    });
}

fn settle_local_port_forward_worker(
    inner: &Arc<LocalPortForwardManagerInner>,
    port: u16,
    mut worker: RemoteWorker,
    deadline: Instant,
) {
    let Some(handle) = worker.handle.as_ref() else {
        return;
    };
    if handle.thread().id() == thread::current().id() {
        defer_local_port_forward_worker(inner, port, worker);
        return;
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    match worker.completion_rx.recv_timeout(remaining) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            let handle = worker.handle.take().expect("local forward worker handle");
            if handle.join().is_err() {
                set_port_forward_state(
                    inner,
                    RemotePortForwardState {
                        port,
                        listener_active: false,
                        local_port_busy: false,
                        message: Some(format!(
                            "Local forward worker {} panicked during shutdown.",
                            worker.name
                        )),
                    },
                );
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            defer_local_port_forward_worker(inner, port, worker)
        }
    }
}

pub(in crate::remote) fn settle_unowned_remote_worker(mut worker: RemoteWorker, deadline: Instant) {
    let Some(handle) = worker.handle.as_ref() else {
        return;
    };
    if handle.thread().id() == thread::current().id() {
        defer_unowned_remote_worker(worker);
        return;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    match worker.completion_rx.recv_timeout(remaining) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            let handle = worker.handle.take().expect("remote worker handle");
            let _ = handle.join();
        }
        Err(mpsc::RecvTimeoutError::Timeout) => defer_unowned_remote_worker(worker),
    }
}

fn settle_remote_client_worker(
    inner: Option<Arc<RemoteClientInner>>,
    mut worker: RemoteWorker,
    deadline: Instant,
) {
    let Some(handle) = worker.handle.as_ref() else {
        return;
    };
    if handle.thread().id() == thread::current().id() {
        if let Some(inner) = inner.as_ref() {
            if let Ok(mut message) = inner.disconnected_message.write() {
                *message = Some(format!(
                    "Remote client worker residue: {} could not join itself; DevManager retained its handle.",
                    worker.name
                ));
            }
        }
        defer_unowned_remote_worker(worker);
        return;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    match worker.completion_rx.recv_timeout(remaining) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            let handle = worker.handle.take().expect("remote client worker handle");
            if handle.join().is_err() {
                if let Some(inner) = inner.as_ref() {
                    if let Ok(mut message) = inner.disconnected_message.write() {
                        *message = Some(format!(
                            "Remote client worker {} panicked during shutdown.",
                            worker.name
                        ));
                    }
                }
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if let Some(inner) = inner.as_ref() {
                if let Ok(mut message) = inner.disconnected_message.write() {
                    *message = Some(format!(
                        "Remote client worker residue: {} did not stop within {} ms; DevManager retained its handle.",
                        worker.name,
                        REMOTE_WORKER_SHUTDOWN_TIMEOUT.as_millis()
                    ));
                }
            }
            defer_unowned_remote_worker(worker);
        }
    }
}

fn reap_completed_local_port_forward_workers(inner: &Arc<LocalPortForwardManagerInner>) {
    let completed = {
        let Ok(mut registry) = inner.worker_registry.lock() else {
            return;
        };
        let completed_ids = registry
            .connections
            .iter()
            .filter_map(|(connection_id, worker)| {
                worker
                    .worker
                    .handle
                    .as_ref()
                    .is_some_and(|handle| handle.is_finished())
                    .then_some(*connection_id)
            })
            .collect::<Vec<_>>();
        completed_ids
            .into_iter()
            .filter_map(|connection_id| registry.connections.remove(&connection_id))
            .collect::<Vec<_>>()
    };
    for mut connection in completed {
        if let Some(handle) = connection.worker.handle.take() {
            let _ = handle.join();
        }
    }
}

fn close_local_port_forward_scope(
    inner: &Arc<LocalPortForwardManagerInner>,
    port: u16,
    scope_id: Option<u64>,
) -> Vec<LocalPortForwardConnectionWorker> {
    let Some(scope_id) = scope_id else {
        return Vec::new();
    };
    let Ok(mut registry) = inner.worker_registry.lock() else {
        return Vec::new();
    };
    if registry.active_scopes.get(&port).copied() == Some(scope_id) {
        registry.active_scopes.remove(&port);
    }
    let connection_ids = registry
        .connections
        .iter()
        .filter_map(|(connection_id, worker)| {
            (worker.port == port && worker.scope_id == scope_id).then_some(*connection_id)
        })
        .collect::<Vec<_>>();
    connection_ids
        .into_iter()
        .filter_map(|connection_id| registry.connections.remove(&connection_id))
        .collect()
}

fn settle_all_local_port_forward_connections(
    inner: &Arc<LocalPortForwardManagerInner>,
    deadline: Instant,
) {
    let connections = {
        let Ok(mut registry) = inner.worker_registry.lock() else {
            return;
        };
        registry.active_scopes.clear();
        std::mem::take(&mut registry.connections)
    };
    for connection in connections.into_values() {
        connection.cancellation.cancel();
        settle_local_port_forward_worker(inner, connection.port, connection.worker, deadline);
    }
}

fn start_local_port_forward_listener(
    inner: Arc<LocalPortForwardManagerInner>,
    port: u16,
) -> Result<LocalPortForwardEntry, String> {
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|error| {
        if error.kind() == ErrorKind::AddrInUse {
            format!("Local port {port} is already in use on this machine.")
        } else {
            format!("Could not bind localhost:{port}: {error}")
        }
    })?;
    listener
        .set_nonblocking(false)
        .map_err(|error| format!("Could not configure localhost:{port}: {error}"))?;
    let wakeup = listener.local_addr().ok();
    let scope_id = inner.next_scope_id.fetch_add(1, Ordering::Relaxed);
    inner
        .worker_registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .active_scopes
        .insert(port, scope_id);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let thread_inner = Arc::downgrade(&inner);
    let worker =
        match RemoteWorker::try_spawn(format!("local-forward-listener-{port}"), None, move || {
            run_local_port_forward_listener(thread_inner, port, scope_id, listener, stop_flag)
        }) {
            Ok(worker) => worker,
            Err(error) => {
                inner
                    .worker_registry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .active_scopes
                    .remove(&port);
                return Err(format!(
                    "Local forward listener worker could not start: {error}"
                ));
            }
        };
    Ok(LocalPortForwardEntry {
        scope_id: Some(scope_id),
        stop: Some(stop),
        worker: Some(worker),
        wakeup,
        retry_after_epoch_ms: 0,
    })
}

fn run_local_port_forward_listener(
    inner: Weak<LocalPortForwardManagerInner>,
    port: u16,
    scope_id: u64,
    listener: TcpListener,
    stop_flag: Arc<AtomicBool>,
) {
    if let Some(inner) = inner.upgrade() {
        set_port_forward_state(
            &inner,
            RemotePortForwardState {
                port,
                listener_active: true,
                local_port_busy: false,
                message: Some(format!(
                    "Forwarding http://localhost:{port} to the remote host."
                )),
            },
        );
    }

    while !stop_flag.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((socket, _)) => {
                let Some(strong_inner) = inner.upgrade() else {
                    let _ = socket.shutdown(Shutdown::Both);
                    return;
                };
                #[cfg(test)]
                if let Some(hook) = strong_inner
                    .lifecycle_test_hook
                    .read()
                    .ok()
                    .and_then(|slot| slot.clone())
                {
                    hook(LocalPortForwardLifecycleTestEvent::ConnectionAccepted);
                }
                #[cfg(test)]
                let connection_handler_test_hook = strong_inner
                    .connection_handler_test_hook
                    .read()
                    .ok()
                    .and_then(|slot| slot.clone());
                let mut registry = strong_inner
                    .worker_registry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let scope_is_active = !stop_flag.load(Ordering::Acquire)
                    && registry.active_scopes.get(&port).copied() == Some(scope_id);
                if !scope_is_active {
                    let _ = socket.shutdown(Shutdown::Both);
                    return;
                }
                let connection_inner = Arc::downgrade(&strong_inner);
                let client = strong_inner.client.clone();
                let connection_stop_flag = stop_flag.clone();
                let cancellation = Arc::new(ForwardCancellation::default());
                if !cancellation.register(&socket) {
                    let _ = socket.shutdown(Shutdown::Both);
                    return;
                }
                let worker_cancellation = cancellation.clone();
                let connection_id = strong_inner
                    .next_connection_id
                    .fetch_add(1, Ordering::Relaxed);
                let worker = match RemoteWorker::try_spawn(
                    format!("local-forward-{port}-{connection_id}"),
                    None,
                    move || {
                        #[cfg(test)]
                        if let Some(hook) = connection_handler_test_hook {
                            hook(port, socket, connection_stop_flag);
                            return;
                        }
                        handle_local_port_forward_connection(
                            connection_inner,
                            client,
                            port,
                            socket,
                            connection_stop_flag,
                            worker_cancellation,
                        )
                    },
                ) {
                    Ok(worker) => worker,
                    Err(error) => {
                        cancellation.cancel();
                        set_port_forward_state(
                            &strong_inner,
                            RemotePortForwardState {
                                port,
                                listener_active: true,
                                local_port_busy: false,
                                message: Some(format!(
                                    "Local forward connection worker unavailable: {error}"
                                )),
                            },
                        );
                        drop(registry);
                        continue;
                    }
                };
                registry.connections.insert(
                    connection_id,
                    LocalPortForwardConnectionWorker {
                        port,
                        scope_id,
                        cancellation,
                        worker,
                    },
                );
                drop(registry);
                reap_completed_local_port_forward_workers(&strong_inner);
            }
            Err(error) => {
                if let Some(inner) = inner.upgrade() {
                    set_port_forward_state(
                        &inner,
                        RemotePortForwardState {
                            port,
                            listener_active: false,
                            local_port_busy: false,
                            message: Some(format!(
                                "Local forward listener on {port} failed: {error}"
                            )),
                        },
                    );
                }
                return;
            }
        }
    }
}

fn handle_local_port_forward_connection(
    inner: Weak<LocalPortForwardManagerInner>,
    client: RemoteClientHandle,
    port: u16,
    mut local_socket: TcpStream,
    stop_flag: Arc<AtomicBool>,
    cancellation: Arc<ForwardCancellation>,
) {
    let _ = local_socket.set_nodelay(true);
    let _ = local_socket.set_read_timeout(None);
    let _ = local_socket.set_write_timeout(None);
    let mut remote_stream = match client.open_port_forward_with_cancellation(port, &cancellation) {
        Ok(stream) => stream,
        Err(error) => {
            if let Some(inner) = inner.upgrade() {
                set_port_forward_state(
                    &inner,
                    RemotePortForwardState {
                        port,
                        listener_active: true,
                        local_port_busy: false,
                        message: Some(format!("Tunnel error on localhost:{port}: {error}")),
                    },
                );
            }
            let _ = local_socket.shutdown(Shutdown::Both);
            return;
        }
    };
    let _ = remote_stream.sock.set_read_timeout(None);
    let _ = remote_stream.sock.set_write_timeout(None);

    if let Err(error) =
        copy_bidirectional(&mut local_socket, &mut remote_stream, &cancellation, || {
            stop_flag.load(Ordering::Acquire)
        })
    {
        if let Some(inner) = inner.upgrade() {
            set_port_forward_state(
                &inner,
                RemotePortForwardState {
                    port,
                    listener_active: true,
                    local_port_busy: false,
                    message: Some(format!("Tunnel error on localhost:{port}: {error}")),
                },
            );
        }
    }
    let _ = local_socket.shutdown(Shutdown::Both);
    let _ = remote_stream.sock.shutdown(Shutdown::Both);
}

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, RawSocket};

trait RemoteForwardStream: Read + Write {
    fn set_forward_nonblocking(&self, nonblocking: bool) -> std::io::Result<()>;

    #[cfg(unix)]
    fn raw_forward_socket(&self) -> RawFd;
    #[cfg(windows)]
    fn raw_forward_socket(&self) -> RawSocket;
}

impl RemoteForwardStream for TcpStream {
    fn set_forward_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        self.set_nonblocking(nonblocking)
    }

    #[cfg(unix)]
    fn raw_forward_socket(&self) -> RawFd {
        self.as_raw_fd()
    }
    #[cfg(windows)]
    fn raw_forward_socket(&self) -> RawSocket {
        self.as_raw_socket()
    }
}

impl RemoteForwardStream for transport::ClientTlsStream {
    fn set_forward_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        self.sock.set_nonblocking(nonblocking)
    }

    #[cfg(unix)]
    fn raw_forward_socket(&self) -> RawFd {
        self.sock.as_raw_fd()
    }
    #[cfg(windows)]
    fn raw_forward_socket(&self) -> RawSocket {
        self.sock.as_raw_socket()
    }
}

impl RemoteForwardStream for transport::ServerTlsStream {
    fn set_forward_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        self.sock.set_nonblocking(nonblocking)
    }

    #[cfg(unix)]
    fn raw_forward_socket(&self) -> RawFd {
        self.sock.as_raw_fd()
    }
    #[cfg(windows)]
    fn raw_forward_socket(&self) -> RawSocket {
        self.sock.as_raw_socket()
    }
}

fn wait_for_remote_socket_io(
    socket: &TcpStream,
    deadline: Instant,
    writable: bool,
) -> std::io::Result<bool> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(false);
    }
    let timeout = remaining.as_millis().min(i32::MAX as u128).max(1) as i32;
    #[cfg(unix)]
    {
        #[repr(C)]
        struct PollFd {
            fd: RawFd,
            events: i16,
            revents: i16,
        }
        unsafe extern "C" {
            fn poll(fds: *mut PollFd, nfds: usize, timeout: i32) -> i32;
        }
        let mut fd = PollFd {
            fd: socket.as_raw_fd(),
            events: if writable { 0x0004 } else { 0x0001 },
            revents: 0,
        };
        let result = unsafe { poll(&mut fd, 1, timeout) };
        if result > 0 {
            Ok(true)
        } else if result == 0 {
            Ok(false)
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        struct WsapollFd {
            fd: RawSocket,
            events: i16,
            revents: i16,
        }
        unsafe extern "system" {
            fn WSAPoll(fds: *mut WsapollFd, nfds: u32, timeout: i32) -> i32;
        }
        let mut fd = WsapollFd {
            fd: socket.as_raw_socket(),
            events: if writable { 0x0010 } else { 0x0300 },
            revents: 0,
        };
        let result = unsafe { WSAPoll(&mut fd, 1, timeout) };
        if result > 0 {
            Ok(true)
        } else if result == 0 {
            Ok(false)
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

fn wait_for_forward_io<L: RemoteForwardStream, R: RemoteForwardStream>(
    left: &L,
    right: &R,
    left_writable: bool,
    right_writable: bool,
    timeout: Duration,
) -> std::io::Result<()> {
    let timeout = timeout.as_millis().min(i32::MAX as u128).max(1) as i32;
    #[cfg(unix)]
    {
        #[repr(C)]
        struct PollFd {
            fd: RawFd,
            events: i16,
            revents: i16,
        }
        unsafe extern "C" {
            fn poll(fds: *mut PollFd, nfds: usize, timeout: i32) -> i32;
        }
        let mut fds = [
            PollFd {
                fd: left.raw_forward_socket(),
                events: if left_writable { 0x0004 } else { 0x0001 },
                revents: 0,
            },
            PollFd {
                fd: right.raw_forward_socket(),
                events: if right_writable { 0x0004 } else { 0x0001 },
                revents: 0,
            },
        ];
        let result = unsafe { poll(fds.as_mut_ptr(), fds.len(), timeout) };
        if result > 0 {
            Ok(())
        } else if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        struct WsapollFd {
            fd: RawSocket,
            events: i16,
            revents: i16,
        }
        unsafe extern "system" {
            fn WSAPoll(fds: *mut WsapollFd, nfds: u32, timeout: i32) -> i32;
        }
        let mut fds = [
            WsapollFd {
                fd: left.raw_forward_socket(),
                events: if left_writable { 0x0010 } else { 0x0300 },
                revents: 0,
            },
            WsapollFd {
                fd: right.raw_forward_socket(),
                events: if right_writable { 0x0010 } else { 0x0300 },
                revents: 0,
            },
        ];
        let result = unsafe { WSAPoll(fds.as_mut_ptr(), fds.len() as u32, timeout) };
        if result >= 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

fn copy_bidirectional<L: RemoteForwardStream, R: RemoteForwardStream>(
    left: &mut L,
    right: &mut R,
    cancellation: &ForwardCancellation,
    mut should_stop: impl FnMut() -> bool,
) -> Result<(), String> {
    let mut left_buf = [0_u8; 16 * 1024];
    let mut right_buf = [0_u8; 16 * 1024];
    let mut left_to_right = Vec::new();
    let mut left_to_right_offset = 0_usize;
    let mut right_flush_pending = false;
    let mut right_to_left = Vec::new();
    let mut right_to_left_offset = 0_usize;
    let mut left_flush_pending = false;
    left.set_forward_nonblocking(true)
        .map_err(|error| format!("Could not configure forward read readiness: {error}"))?;
    right
        .set_forward_nonblocking(true)
        .map_err(|error| format!("Could not configure forward read readiness: {error}"))?;
    loop {
        if cancellation.is_cancelled() || should_stop() {
            break;
        }
        let mut made_progress = false;
        if left_to_right_offset < left_to_right.len() || right_flush_pending {
            let write_result = if left_to_right_offset < left_to_right.len() {
                right.write(&left_to_right[left_to_right_offset..])
            } else {
                Ok(0)
            };
            match write_result {
                Ok(0) if left_to_right_offset == left_to_right.len() => {}
                Ok(0) => return Err("Write failed: forwarded socket accepted zero bytes".into()),
                Ok(written) => {
                    left_to_right_offset += written;
                    right_flush_pending = true;
                    made_progress = true;
                    if left_to_right_offset == left_to_right.len() {
                        left_to_right.clear();
                        left_to_right_offset = 0;
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    #[cfg(test)]
                    cancellation.notify_write_blocked();
                }
                Err(error) => {
                    if cancellation.is_cancelled() || should_stop() {
                        break;
                    }
                    return Err(format!("Write failed: {error}"));
                }
            }
            if left_to_right_offset == left_to_right.len() && right_flush_pending {
                match right.flush() {
                    Ok(()) => {
                        right_flush_pending = false;
                        made_progress = true;
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        #[cfg(test)]
                        cancellation.notify_write_blocked();
                    }
                    Err(error) => {
                        if cancellation.is_cancelled() || should_stop() {
                            break;
                        }
                        return Err(format!("Flush failed: {error}"));
                    }
                }
            }
        } else {
            match left.read(&mut left_buf) {
                Ok(0) => break,
                Ok(read) => {
                    left_to_right.extend_from_slice(&left_buf[..read]);
                    made_progress = true;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => {
                    if cancellation.is_cancelled() || should_stop() {
                        break;
                    }
                    return Err(format!("Read failed: {error}"));
                }
            }
        }

        if cancellation.is_cancelled() || should_stop() {
            break;
        }
        if right_to_left_offset < right_to_left.len() || left_flush_pending {
            let write_result = if right_to_left_offset < right_to_left.len() {
                left.write(&right_to_left[right_to_left_offset..])
            } else {
                Ok(0)
            };
            match write_result {
                Ok(0) if right_to_left_offset == right_to_left.len() => {}
                Ok(0) => return Err("Write failed: forwarded socket accepted zero bytes".into()),
                Ok(written) => {
                    right_to_left_offset += written;
                    left_flush_pending = true;
                    made_progress = true;
                    if right_to_left_offset == right_to_left.len() {
                        right_to_left.clear();
                        right_to_left_offset = 0;
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    #[cfg(test)]
                    cancellation.notify_write_blocked();
                }
                Err(error) => {
                    if cancellation.is_cancelled() || should_stop() {
                        break;
                    }
                    return Err(format!("Write failed: {error}"));
                }
            }
            if right_to_left_offset == right_to_left.len() && left_flush_pending {
                match left.flush() {
                    Ok(()) => {
                        left_flush_pending = false;
                        made_progress = true;
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        #[cfg(test)]
                        cancellation.notify_write_blocked();
                    }
                    Err(error) => {
                        if cancellation.is_cancelled() || should_stop() {
                            break;
                        }
                        return Err(format!("Flush failed: {error}"));
                    }
                }
            }
        } else {
            match right.read(&mut right_buf) {
                Ok(0) => break,
                Ok(read) => {
                    right_to_left.extend_from_slice(&right_buf[..read]);
                    made_progress = true;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => {
                    if cancellation.is_cancelled() || should_stop() {
                        break;
                    }
                    return Err(format!("Read failed: {error}"));
                }
            }
        }
        if !made_progress {
            let wait_result = wait_for_forward_io(
                left,
                right,
                right_to_left_offset < right_to_left.len() || left_flush_pending,
                left_to_right_offset < left_to_right.len() || right_flush_pending,
                Duration::from_millis(25),
            );
            if let Err(error) = wait_result {
                if cancellation.is_cancelled() || should_stop() {
                    break;
                }
                return Err(format!("Forward readiness wait failed: {error}"));
            }
        }
    }
    Ok(())
}

fn native_connection_should_stop(inner: &RemoteHostInner, native_runtime_generation: u64) -> bool {
    inner.stop_flag.load(Ordering::Acquire)
        || inner.native_runtime_generation.load(Ordering::Acquire) != native_runtime_generation
}

fn native_connection_should_stop_weak(
    inner: &Weak<RemoteHostInner>,
    native_runtime_generation: u64,
) -> bool {
    inner
        .upgrade()
        .map(|inner| native_connection_should_stop(&inner, native_runtime_generation))
        .unwrap_or(true)
}

#[cfg(test)]
fn notify_native_lifecycle(inner: &Arc<RemoteHostInner>, event: NativeLifecycleTestEvent) {
    let hook = inner
        .native_lifecycle_test_hook
        .read()
        .ok()
        .and_then(|slot| slot.clone());
    if let Some(hook) = hook {
        hook(event);
    }
}

#[cfg(test)]
pub(crate) fn notify_client_registration(
    inner: &Arc<RemoteHostInner>,
    event: ClientRegistrationTestEvent,
) {
    let hook = inner
        .client_registration_test_hook
        .read()
        .ok()
        .and_then(|slot| slot.clone());
    if let Some(hook) = hook {
        hook(event);
    }
}

fn spawn_native_connection_worker(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    stream: TcpStream,
    native_runtime_generation: u64,
) {
    // The cancellation owner is shared by the registry and worker before the
    // thread starts. A restart can therefore close the accepted endpoint even
    // while worker admission itself is paused, and a port-forward worker adds
    // its upstream endpoint to the same owner immediately after connect.
    let cancellation = Arc::new(ForwardCancellation::default());
    if !cancellation.register(&stream) {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    let done = Arc::new(AtomicBool::new(false));
    // The stalled TLS phase must not keep the host runtime alive after the
    // owner has revoked the generation. Upgrade this weak reference only for
    // the post-handshake work that needs the live service.
    let thread_inner = Arc::downgrade(inner);
    let worker_cancellation = cancellation.clone();
    let worker = match RemoteWorker::try_spawn(
        format!("remote-native-{connection_id}"),
        Some(done.clone()),
        move || {
            handle_client_connection_with_weak(
                thread_inner,
                connection_id,
                stream,
                native_runtime_generation,
                worker_cancellation,
            );
        },
    ) {
        Ok(worker) => worker,
        Err(error) => {
            cancellation.cancel();
            set_last_connection_note(
                inner,
                format!(
                    "Remote native connection worker could not start: {error}; accepted connection was rejected."
                ),
                true,
            );
            return;
        }
    };
    #[cfg(test)]
    if let Some(hook) = inner
        .native_worker_registration_test_hook
        .read()
        .ok()
        .and_then(|slot| slot.clone())
    {
        hook();
    }
    let mut worker = Some(worker);
    {
        let _lifecycle_guard = inner
            .lifecycle_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !native_connection_should_stop(inner, native_runtime_generation) {
            inner
                .native_connection_workers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(
                    connection_id,
                    NativeConnectionWorker {
                        generation: native_runtime_generation,
                        done,
                        cancellation: cancellation.clone(),
                        worker: worker.take().expect("native worker should register once"),
                    },
                );
        }
    }
    if let Some(worker) = worker {
        cancellation.cancel();
        settle_remote_worker(
            inner,
            worker,
            Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT,
        );
    }
}

fn reap_completed_native_connection_workers(inner: &Arc<RemoteHostInner>) {
    let completed = {
        let mut workers = inner
            .native_connection_workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let completed_ids = workers
            .iter()
            .filter_map(|(connection_id, worker)| {
                worker
                    .done
                    .load(Ordering::Acquire)
                    .then_some(*connection_id)
            })
            .collect::<Vec<_>>();
        completed_ids
            .into_iter()
            .filter_map(|connection_id| workers.remove(&connection_id))
            .collect::<Vec<_>>()
    };
    for worker in completed {
        settle_remote_worker(
            inner,
            worker.worker,
            Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT,
        );
    }
}

fn join_native_connection_workers_before_generation(
    inner: &Arc<RemoteHostInner>,
    generation: u64,
    deadline: Instant,
) {
    let workers = {
        let mut workers = inner
            .native_connection_workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stopped_ids = workers
            .iter()
            .filter_map(|(connection_id, worker)| {
                (worker.generation < generation).then_some(*connection_id)
            })
            .collect::<Vec<_>>();
        stopped_ids
            .into_iter()
            .filter_map(|connection_id| workers.remove(&connection_id))
            .collect::<Vec<_>>()
    };
    for worker in workers {
        worker.cancellation.cancel();
        settle_remote_worker(inner, worker.worker, deadline);
    }
}

fn cancel_native_connection_workers_before_generation(
    inner: &Arc<RemoteHostInner>,
    generation: u64,
) {
    let workers = inner
        .native_connection_workers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for worker in workers.values() {
        if worker.generation < generation {
            worker.cancellation.cancel();
        }
    }
}

fn run_listener(
    inner: Weak<RemoteHostInner>,
    native_runtime_generation: u64,
    lease: Option<ListenerLease>,
) {
    let Some(lease) = lease else {
        return;
    };
    if !lease.is_current() {
        return;
    }
    let Some(runtime) = inner.upgrade() else {
        return;
    };
    let config = runtime
        .config
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let bind = format!("{}:{}", config.bind_address, config.port);
    let listener = match TcpListener::bind(&bind) {
        Ok(listener) => listener,
        Err(error) => {
            let failure = ListenerBindFailure::from_io(bind.clone(), error);
            runtime.listener_running.store(false, Ordering::Relaxed);
            if let Ok(mut slot) = runtime.listener_error.write() {
                *slot = Some(failure.to_string());
            }
            set_last_connection_note(
                &runtime,
                format!("Remote host could not start listening: {failure}"),
                true,
            );
            eprintln!("[remote] failed to bind {failure}");
            #[cfg(test)]
            notify_native_lifecycle(&runtime, NativeLifecycleTestEvent::ListenerBindFailed);
            return;
        }
    };
    if !lease.is_current() {
        let failure = ListenerBindFailure::GenerationStale {
            bind: bind.clone(),
            phase: "after",
        };
        if let Ok(mut slot) = runtime.listener_error.write() {
            *slot = Some(failure.to_string());
        }
        let _ = listener.set_nonblocking(false);
        return;
    }
    runtime.listener_running.store(true, Ordering::Relaxed);
    if let Ok(mut slot) = runtime.listener_error.write() {
        *slot = None;
    }
    let _ = listener.set_nonblocking(false);
    if let Ok(local_addr) = listener.local_addr() {
        let wake_addr = match local_addr {
            SocketAddr::V4(addr) if addr.ip().is_unspecified() => SocketAddr::V4(
                std::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, addr.port()),
            ),
            SocketAddr::V6(addr) if addr.ip().is_unspecified() => {
                SocketAddr::V6(std::net::SocketAddrV6::new(
                    Ipv6Addr::LOCALHOST,
                    addr.port(),
                    addr.flowinfo(),
                    addr.scope_id(),
                ))
            }
            addr => addr,
        };
        if let Ok(mut slot) = runtime.native_listener_wakeup.lock() {
            *slot = Some(wake_addr);
        }
    }
    #[cfg(test)]
    notify_native_lifecycle(&runtime, NativeLifecycleTestEvent::ListenerStarted);
    drop(runtime);

    loop {
        let Some(runtime) = inner.upgrade() else {
            break;
        };
        if native_connection_should_stop(&runtime, native_runtime_generation) {
            break;
        }
        reap_completed_native_connection_workers(&runtime);
        drop(runtime);
        match listener.accept() {
            Ok((stream, _)) => {
                let Some(runtime) = inner.upgrade() else {
                    let _ = stream.shutdown(Shutdown::Both);
                    break;
                };
                if native_connection_should_stop(&runtime, native_runtime_generation) {
                    let _ = stream.shutdown(Shutdown::Both);
                    break;
                }
                let connection_id = runtime.next_connection_id.fetch_add(1, Ordering::Relaxed);
                spawn_native_connection_worker(
                    &runtime,
                    connection_id,
                    stream,
                    native_runtime_generation,
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                if let Some(runtime) = inner.upgrade() {
                    if !native_connection_should_stop(&runtime, native_runtime_generation) {
                        if let Ok(mut slot) = runtime.listener_error.write() {
                            *slot = Some(format!("Remote listener accept failed: {error}"));
                        }
                    }
                }
                break;
            }
        }
    }
    if let Some(runtime) = inner.upgrade() {
        if let Ok(mut slot) = runtime.native_listener_wakeup.lock() {
            *slot = None;
        }
        runtime.listener_running.store(false, Ordering::Relaxed);
    }
}

fn run_broadcaster(
    inner: Weak<RemoteHostInner>,
    signal: Arc<(Mutex<u64>, Condvar)>,
    native_runtime_generation: u64,
) {
    let mut last_snapshot_revision = 0_u64;
    let mut last_semantic_delivery_revision = 0_u64;
    let mut last_controller_client_id: Option<String> = None;
    let mut last_bootstrap_retry_at: HashMap<String, Instant> = HashMap::new();

    loop {
        let Some(inner) = inner.upgrade() else {
            break;
        };
        if native_connection_should_stop(&inner, native_runtime_generation) {
            break;
        }
        reap_completed_native_connection_workers(&inner);
        let connected_clients = inner
            .clients
            .lock()
            .map(|clients| clients.len())
            .unwrap_or(0);
        if connected_clients == 0 {
            drop(inner);
            wait_for_broadcaster_signal(&signal, IDLE_BROADCAST_INTERVAL);
            continue;
        }

        deliver_pending_bootstraps_for_generation(
            &inner,
            &mut last_bootstrap_retry_at,
            native_runtime_generation,
        );

        let snapshot_revision = inner.snapshot_revision.load(Ordering::Relaxed);
        if snapshot_revision != last_semantic_delivery_revision
            && deliver_live_semantic_events(&inner)
        {
            last_semantic_delivery_revision = snapshot_revision;
        }
        let controller_client_id = inner
            .controller_client_id
            .read()
            .map(|slot| slot.clone())
            .unwrap_or_default();
        if snapshot_revision == last_snapshot_revision
            && controller_client_id == last_controller_client_id
        {
            drop(inner);
            wait_for_broadcaster_signal(&signal, SNAPSHOT_BROADCAST_INTERVAL);
            continue;
        }

        let app_state = inner
            .shared_state
            .read()
            .map(|slot| slot.clone())
            .unwrap_or_default();
        let runtime_state = inner
            .runtime_state
            .read()
            .map(|slot| slot.clone())
            .unwrap_or_default();
        let port_statuses = inner
            .port_statuses
            .read()
            .map(|slot| slot.clone())
            .unwrap_or_default();
        let port_authorities = inner
            .port_authorities
            .read()
            .map(|slot| slot.clone())
            .unwrap_or_default();
        let app_hash = stable_hash(&app_state);
        let runtime_hash = stable_hash(&runtime_state);
        let port_hash = stable_hash(&port_statuses);
        let authority_hash = stable_hash(&port_authorities);
        let combined_port_hash = port_hash ^ authority_hash;

        let Ok(mut clients) = inner.clients.lock() else {
            drop(inner);
            wait_for_broadcaster_signal(&signal, SNAPSHOT_BROADCAST_INTERVAL);
            continue;
        };
        let mut deliveries = Vec::new();

        for (connection_id, client) in clients.iter_mut() {
            let you_have_control =
                controller_client_id.as_deref() == Some(client.client_id.as_str());
            let app_changed = client.last_app_hash != app_hash;
            let runtime_changed = client.last_runtime_hash != runtime_hash;
            let port_changed = client.last_port_hash != combined_port_hash;
            let controller_changed = client.last_controller_client_id != controller_client_id
                || client.last_you_have_control != you_have_control;
            let web_revision_changed =
                client.web_sender.is_some() && client.last_snapshot_revision != snapshot_revision;

            if !app_changed
                && !runtime_changed
                && !port_changed
                && !controller_changed
                && !web_revision_changed
            {
                continue;
            }

            let delta = RemoteWorkspaceDelta {
                app_state: app_changed.then_some(app_state.clone()),
                runtime_state: runtime_changed.then_some(runtime_state.clone()),
                port_statuses: port_changed.then_some(port_statuses.clone()),
                port_authorities: port_changed.then_some(port_authorities.clone()),
                controller_client_id: controller_client_id.clone(),
                you_have_control,
            };

            client.last_app_hash = app_hash;
            client.last_runtime_hash = runtime_hash;
            client.last_port_hash = combined_port_hash;
            client.last_controller_client_id = controller_client_id.clone();
            client.last_you_have_control = you_have_control;
            client.last_snapshot_revision = snapshot_revision;
            if let Some(target) = client_delivery_target(client) {
                deliveries.push((*connection_id, target, ServerMessage::Delta { delta }));
            }
        }
        drop(clients);
        for (connection_id, target, message) in deliveries {
            if !deliver_server_message(&inner, connection_id, &target, message) {
                revoke_failed_delivery(&inner, connection_id, target);
            }
        }

        last_snapshot_revision = snapshot_revision;
        last_controller_client_id = controller_client_id;

        drop(inner);
        wait_for_broadcaster_signal(&signal, SNAPSHOT_BROADCAST_INTERVAL);
    }
}

#[cfg(test)]
pub(crate) fn deliver_pending_bootstraps(
    inner: &Arc<RemoteHostInner>,
    last_bootstrap_retry_at: &mut HashMap<String, Instant>,
) {
    let generation = inner.native_runtime_generation.load(Ordering::Acquire);
    deliver_pending_bootstraps_for_generation(inner, last_bootstrap_retry_at, generation);
}

fn deliver_pending_bootstraps_for_generation(
    inner: &Arc<RemoteHostInner>,
    last_bootstrap_retry_at: &mut HashMap<String, Instant>,
    native_runtime_generation: u64,
) {
    // Retry pending bootstraps from the broadcaster thread instead of the PTY
    // output path. That keeps terminal output flowing immediately and rate-
    // limits repeated snapshot attempts for hot AI sessions until one
    // bootstrap finally succeeds.
    let pending_session_ids: HashSet<String> = {
        let Ok(clients) = inner.clients.lock() else {
            return;
        };
        clients
            .values()
            .flat_map(|client| {
                client
                    .bootstrap_pending_session_ids
                    .iter()
                    .filter(|session_id| {
                        client.subscribed_session_ids.contains(*session_id)
                            && !client.bootstrapped_session_ids.contains(*session_id)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    if pending_session_ids.is_empty() {
        last_bootstrap_retry_at.clear();
        return;
    }

    last_bootstrap_retry_at.retain(|session_id, _| pending_session_ids.contains(session_id));
    let now = Instant::now();
    let due_session_ids: Vec<String> = pending_session_ids
        .into_iter()
        .filter(|session_id| {
            last_bootstrap_retry_at
                .get(session_id)
                .map(|last_retry| {
                    now.duration_since(*last_retry) >= PENDING_BOOTSTRAP_RETRY_INTERVAL
                })
                .unwrap_or(true)
        })
        .collect();
    if due_session_ids.is_empty() {
        return;
    }

    let provider = inner
        .session_bootstrap_provider
        .read()
        .ok()
        .and_then(|slot| slot.as_ref().cloned());
    let Some(provider) = provider else {
        return;
    };

    for session_id in &due_session_ids {
        last_bootstrap_retry_at.insert(session_id.clone(), now);
    }
    let due_session_ids_set: HashSet<String> = due_session_ids.iter().cloned().collect();
    let bootstraps: HashMap<String, RemoteSessionBootstrap> = due_session_ids
        .iter()
        .filter_map(|session_id| {
            run_bounded_bootstrap_provider(
                inner,
                provider.clone(),
                session_id,
                native_runtime_generation,
            )
            .map(|bootstrap| (session_id.clone(), bootstrap))
        })
        .collect();
    if bootstraps.is_empty() {
        return;
    }
    let Ok(mut clients) = inner.clients.lock() else {
        return;
    };
    let mut deliveries = Vec::new();
    for (connection_id, client) in clients.iter_mut() {
        let pending_for_client: Vec<String> = client
            .bootstrap_pending_session_ids
            .iter()
            .cloned()
            .collect();
        for session_id in pending_for_client {
            if !due_session_ids_set.contains(&session_id) {
                continue;
            }
            if !client.subscribed_session_ids.contains(&session_id)
                || client.bootstrapped_session_ids.contains(&session_id)
            {
                client.bootstrap_pending_session_ids.remove(&session_id);
                continue;
            }
            let Some(bootstrap) = bootstraps.get(&session_id) else {
                continue;
            };
            if let Some(target) = client_delivery_target(client) {
                deliveries.push((
                    *connection_id,
                    target,
                    session_id,
                    ServerMessage::SessionStream {
                        event: RemoteSessionStreamEvent::Bootstrap {
                            bootstrap: bootstrap.clone(),
                        },
                    },
                ));
            }
        }
    }
    drop(clients);
    for (connection_id, target, session_id, message) in deliveries {
        if deliver_server_message(inner, connection_id, &target, message) {
            if let Ok(mut clients) = inner.clients.lock() {
                if let Some(client) = clients.get_mut(&connection_id) {
                    client.bootstrap_pending_session_ids.remove(&session_id);
                    client.bootstrapped_session_ids.insert(session_id);
                }
            }
        } else {
            revoke_failed_delivery(inner, connection_id, target);
        }
    }
}

fn run_bounded_bootstrap_provider(
    inner: &Arc<RemoteHostInner>,
    provider: SessionBootstrapProvider,
    session_id: &str,
    native_runtime_generation: u64,
) -> Option<RemoteSessionBootstrap> {
    if native_connection_should_stop(inner, native_runtime_generation) {
        return None;
    }
    let Some(permit) = inner.host_work_limiter.try_acquire() else {
        set_last_connection_note(
            inner,
            "Remote bootstrap callback capacity is exhausted; retrying shortly.".to_string(),
            true,
        );
        return None;
    };
    let callback_session_id = session_id.to_string();
    let worker_name = format!(
        "remote-bootstrap-{:016x}",
        stable_hash(&callback_session_id)
    );
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let worker = match RemoteWorker::try_spawn(worker_name.clone(), None, move || {
        let result = permit.run(|| provider(&callback_session_id));
        let _ = result_tx.try_send(result);
    }) {
        Ok(worker) => worker,
        Err(error) => {
            set_last_connection_note(
                inner,
                format!("Remote bootstrap callback worker unavailable: {error}"),
                true,
            );
            return None;
        }
    };

    match receive_remote_callback_until_cancelled(
        &result_rx,
        Instant::now() + REMOTE_CALLBACK_TIMEOUT,
        || native_connection_should_stop(inner, native_runtime_generation),
    ) {
        Ok(result) => {
            settle_remote_worker(
                inner,
                worker,
                Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT,
            );
            if native_connection_should_stop(inner, native_runtime_generation) {
                None
            } else {
                result
            }
        }
        Err(RemoteCallbackWaitError::Timeout | RemoteCallbackWaitError::Cancelled) => {
            settle_remote_worker(inner, worker, Instant::now());
            None
        }
        Err(RemoteCallbackWaitError::Disconnected) => {
            settle_remote_worker(
                inner,
                worker,
                Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT,
            );
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteCallbackWaitError {
    Timeout,
    Cancelled,
    Disconnected,
}

fn receive_remote_callback_until_cancelled<T>(
    receiver: &mpsc::Receiver<T>,
    deadline: Instant,
    mut is_cancelled: impl FnMut() -> bool,
) -> Result<T, RemoteCallbackWaitError> {
    loop {
        if is_cancelled() {
            return Err(RemoteCallbackWaitError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(RemoteCallbackWaitError::Timeout);
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(25))) {
            Ok(result) => return Ok(result),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(RemoteCallbackWaitError::Disconnected)
            }
        }
    }
}

fn run_bounded_remote_callback<T, F>(
    inner: &Arc<RemoteHostInner>,
    native_runtime_generation: u64,
    name: impl Into<String>,
    callback: F,
) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    if native_connection_should_stop(inner, native_runtime_generation) {
        return None;
    }
    let Some(permit) = inner.host_work_limiter.try_acquire() else {
        set_last_connection_note(
            inner,
            "Remote callback capacity is exhausted; callback was deferred.".to_string(),
            true,
        );
        return None;
    };
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let worker = match RemoteWorker::try_spawn(name, None, move || {
        let result = permit.run(callback);
        let _ = result_tx.try_send(result);
    }) {
        Ok(worker) => worker,
        Err(error) => {
            set_last_connection_note(
                inner,
                format!("Remote callback worker unavailable: {error}"),
                true,
            );
            return None;
        }
    };
    match receive_remote_callback_until_cancelled(
        &result_rx,
        Instant::now() + REMOTE_CALLBACK_TIMEOUT,
        || native_connection_should_stop(inner, native_runtime_generation),
    ) {
        Ok(result) => {
            settle_remote_worker(
                inner,
                worker,
                Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT,
            );
            if native_connection_should_stop(inner, native_runtime_generation) {
                None
            } else {
                Some(result)
            }
        }
        Err(RemoteCallbackWaitError::Timeout | RemoteCallbackWaitError::Cancelled) => {
            set_last_connection_note(
                inner,
                "Remote callback exceeded its bounded deadline; worker remains owned for cooperative shutdown."
                    .to_string(),
                true,
            );
            settle_remote_worker(inner, worker, Instant::now());
            None
        }
        Err(RemoteCallbackWaitError::Disconnected) => {
            settle_remote_worker(
                inner,
                worker,
                Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT,
            );
            None
        }
    }
}

fn handle_client_connection(
    inner: Arc<RemoteHostInner>,
    connection_id: u64,
    stream: TcpStream,
    native_runtime_generation: u64,
) {
    let cancellation = Arc::new(ForwardCancellation::default());
    if !cancellation.register(&stream) {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    let inner = Arc::downgrade(&inner);
    handle_client_connection_with_weak(
        inner,
        connection_id,
        stream,
        native_runtime_generation,
        cancellation,
    );
}

fn handle_client_connection_with_weak(
    inner: Weak<RemoteHostInner>,
    connection_id: u64,
    stream: TcpStream,
    native_runtime_generation: u64,
    cancellation: Arc<ForwardCancellation>,
) {
    let peer_addr = stream.peer_addr().ok();
    let peer_label = peer_addr
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| "unknown client".to_string());
    let peer_ip = peer_addr.map(|addr| addr.ip().to_string());
    let Some(config) = inner.upgrade().map(|inner| {
        inner
            .config
            .read()
            .map(|slot| slot.clone())
            .unwrap_or_default()
    }) else {
        return;
    };
    let handshake_deadline = Instant::now() + Duration::from_secs(5);
    let mut stream =
        match transport::accept_tls_with_deadline(stream, &config, handshake_deadline, || {
            native_connection_should_stop_weak(&inner, native_runtime_generation)
        }) {
            Ok(result) => result.stream,
            Err(error) => {
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                if native_connection_should_stop(&inner, native_runtime_generation) {
                    return;
                }
                set_last_connection_note(
                    &inner,
                    format!("TLS handshake from {peer_label} failed: {error}"),
                    true,
                );
                eprintln!("[remote] tls accept failed for connection {connection_id}: {error}");
                return;
            }
        };
    let mut read_buffer = Vec::new();

    let _ = stream.sock.set_read_timeout(Some(
        handshake_deadline.saturating_duration_since(Instant::now()),
    ));
    let hello = match read_message_until_deadline_cancelled::<ClientMessage, _, _>(
        &mut stream,
        handshake_deadline,
        || native_connection_should_stop_weak(&inner, native_runtime_generation),
    ) {
        Ok(message) => message,
        Err(error) => {
            let Some(inner) = inner.upgrade() else {
                return;
            };
            if native_connection_should_stop(&inner, native_runtime_generation) {
                return;
            }
            set_last_connection_note(
                &inner,
                format!(
                    "Client {peer_label} disconnected before DevManager handshake completed: {error}"
                ),
                true,
            );
            eprintln!(
                "[remote] handshake read failed for connection {connection_id} from {peer_label}: {error}"
            );
            return;
        }
    };
    let _ = stream.sock.set_read_timeout(None);
    let _ = stream.sock.set_write_timeout(None);

    let Some(inner) = inner.upgrade() else {
        return;
    };
    if matches!(hello, ClientMessage::PortForwardHello { .. }) {
        if let Err(message) = handle_port_forward_connection(
            &inner,
            &peer_label,
            &mut stream,
            hello,
            native_runtime_generation,
            handshake_deadline,
            &cancellation,
        ) {
            set_last_connection_note(
                &inner,
                format!("Rejected port forward from {peer_label}: {message}"),
                true,
            );
            let _ = set_server_handshake_write_deadline(&mut stream, handshake_deadline);
            let _ = write_message_until_deadline(
                &mut stream,
                &ServerMessage::HelloErr { message },
                handshake_deadline,
            );
        }
        return;
    }

    let authentication = match prepare_native_client_authentication(hello, Some(peer_ip.clone()))
        .and_then(|authentication| {
            validate_prepared_native_authentication(&inner, &authentication)?;
            Ok(authentication)
        }) {
        Ok(authentication) => authentication,
        Err(message) => {
            set_last_connection_note(
                &inner,
                format!("Rejected remote client from {peer_label}: {message}"),
                true,
            );
            eprintln!(
                "[remote] handshake rejected for connection {connection_id} from {peer_label}: {message}"
            );
            let _ = set_server_handshake_write_deadline(&mut stream, handshake_deadline);
            let _ = write_message_until_deadline(
                &mut stream,
                &ServerMessage::HelloErr { message },
                handshake_deadline,
            );
            return;
        }
    };
    let client_id = authentication.client_id().to_string();
    if native_connection_should_stop(&inner, native_runtime_generation) {
        return;
    }

    let (tx, rx) = mpsc::channel::<ServerMessage>();
    let native_sender = Arc::new(tx.clone());

    let controller_client_id = inner
        .controller_client_id
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let you_have_control = controller_client_id.as_deref() == Some(client_id.as_str());
    let snapshot = light_snapshot(&inner, &client_id);
    let app_hash = stable_hash(&snapshot.app_state);
    let runtime_hash = stable_hash(&snapshot.runtime_state);
    let port_hash = stable_hash(&snapshot.port_statuses);
    let authority_hash = stable_hash(&snapshot.port_authorities);
    #[cfg(test)]
    notify_client_registration(&inner, ClientRegistrationTestEvent::BeforeFence);
    let authenticated = admit_native_client(
        &inner,
        native_runtime_generation,
        connection_id,
        &authentication,
        ConnectedRemoteClient {
            client_id: client_id.clone(),
            sender: Some(native_sender.clone()),
            web_sender: None,
            web_tombstone: None,
            semantic_cursors: HashMap::new(),
            subscribed_session_ids: HashSet::new(),
            bootstrapped_session_ids: HashSet::new(),
            bootstrap_pending_session_ids: HashSet::new(),
            focused_session_id: snapshot.runtime_state.active_session_id.clone(),
            last_app_hash: app_hash,
            last_runtime_hash: runtime_hash,
            last_port_hash: port_hash ^ authority_hash,
            last_controller_client_id: controller_client_id.clone(),
            last_you_have_control: you_have_control,
            last_snapshot_revision: inner.snapshot_revision.load(Ordering::Relaxed),
        },
    );
    let (client_id, client_token, _client_label) = match authenticated {
        Ok(Some(authenticated)) => authenticated,
        Ok(None) => {
            #[cfg(test)]
            notify_client_registration(&inner, ClientRegistrationTestEvent::Rejected);
            let _ = stream.sock.shutdown(Shutdown::Both);
            return;
        }
        Err(error) => {
            let message = error.to_string();
            #[cfg(test)]
            notify_client_registration(&inner, ClientRegistrationTestEvent::Rejected);
            set_last_connection_note(
                &inner,
                format!("Rejected remote client from {peer_label}: {message}"),
                true,
            );
            let _ = set_server_handshake_write_deadline(&mut stream, handshake_deadline);
            let _ = write_message_until_deadline(
                &mut stream,
                &ServerMessage::HelloErr { message },
                handshake_deadline,
            );
            return;
        }
    };
    #[cfg(test)]
    notify_client_registration(&inner, ClientRegistrationTestEvent::Registered);
    notify_broadcaster(&inner);
    #[cfg(test)]
    notify_native_lifecycle(&inner, NativeLifecycleTestEvent::ClientRegistered);

    let hello_ok = ServerMessage::HelloOk {
        protocol_version: PROTOCOL_VERSION,
        server_id: config.server_id.clone(),
        certificate_fingerprint: config.certificate_fingerprint.clone(),
        client_id: client_id.clone(),
        client_token: client_token.clone(),
        controller_client_id,
        you_have_control,
        snapshot,
    };
    let _ = set_server_handshake_write_deadline(&mut stream, handshake_deadline);
    if let Err(error) = write_message_until_deadline(&mut stream, &hello_ok, handshake_deadline) {
        set_last_connection_note(
            &inner,
            format!(
                "Remote client {client_id} connected from {peer_label} but the host could not finish the handshake: {error}"
            ),
            true,
        );
        eprintln!(
            "[remote] handshake reply failed for connection {connection_id} ({client_id} from {peer_label}): {error}"
        );
        let removed = remove_exact_native_registration(&inner, connection_id, &native_sender);
        if removed {
            notify_broadcaster(&inner);
        }
        #[cfg(test)]
        if removed {
            notify_native_lifecycle(&inner, NativeLifecycleTestEvent::ClientRemoved);
        }
        return;
    }
    set_last_connection_note(
        &inner,
        format!("Remote client {client_id} connected from {peer_label}."),
        false,
    );

    if let Err(error) = stream.sock.set_nonblocking(true) {
        set_last_connection_note(
            &inner,
            format!("Remote native socket could not enter readiness mode: {error}"),
            true,
        );
        let removed = remove_exact_native_registration(&inner, connection_id, &native_sender);
        if removed {
            notify_broadcaster(&inner);
        }
        #[cfg(test)]
        if removed {
            notify_native_lifecycle(&inner, NativeLifecycleTestEvent::ClientRemoved);
        }
        return;
    }

    let inner_weak = Arc::downgrade(&inner);
    drop(inner);
    loop {
        let Some(inner) = inner_weak.upgrade() else {
            break;
        };
        if native_connection_should_stop(&inner, native_runtime_generation) {
            break;
        }
        let mut should_break = false;
        for _ in 0..MAX_OUTBOUND_MESSAGES_PER_TICK {
            match rx.try_recv() {
                Ok(message) => {
                    let is_disconnect = matches!(message, ServerMessage::Disconnected { .. });
                    if write_message(&mut stream, &message).is_err() {
                        should_break = true;
                        break;
                    }
                    if is_disconnect {
                        should_break = true;
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    should_break = true;
                    break;
                }
            }
        }
        if should_break {
            break;
        }

        // Do not keep the host runtime alive while readiness waits for client
        // input. Teardown drops the last registry hold before joining this
        // worker, so a blocked native socket cannot retain the stopped host.
        drop(inner);
        let incoming = try_read_message::<ClientMessage, _>(&mut stream, &mut read_buffer);
        let Some(inner) = inner_weak.upgrade() else {
            break;
        };
        match incoming {
            Ok(Some(ClientMessage::SetFocusedSession { session_id })) => {
                if let Ok(mut clients) = inner.clients.lock() {
                    if let Some(client) = clients.get_mut(&connection_id) {
                        client.focused_session_id = session_id.clone();
                    }
                }
                if let Some(session_id) = session_id {
                    let handler = inner
                        .focused_session_handler
                        .read()
                        .ok()
                        .and_then(|slot| slot.clone());
                    if let Some(handler) = handler {
                        let _ = run_bounded_remote_callback(
                            &inner,
                            native_runtime_generation,
                            "remote-focused-session-callback",
                            move || handler(session_id),
                        );
                    }
                }
            }
            Ok(Some(ClientMessage::SubscribeSessions { session_ids })) => {
                if let Ok(mut clients) = inner.clients.lock() {
                    if let Some(client) = clients.get_mut(&connection_id) {
                        for session_id in &session_ids {
                            client.subscribed_session_ids.insert(session_id.clone());
                            if !client.bootstrapped_session_ids.contains(session_id) {
                                client
                                    .bootstrap_pending_session_ids
                                    .insert(session_id.clone());
                            }
                        }
                    }
                }
            }
            Ok(Some(ClientMessage::UnsubscribeSessions { session_ids })) => {
                if let Ok(mut clients) = inner.clients.lock() {
                    if let Some(client) = clients.get_mut(&connection_id) {
                        for session_id in &session_ids {
                            client.subscribed_session_ids.remove(session_id);
                            client.bootstrapped_session_ids.remove(session_id);
                            client.bootstrap_pending_session_ids.remove(session_id);
                        }
                    }
                }
            }
            Ok(Some(ClientMessage::Action { action })) => {
                if requires_control(&action) && !current_controller_allows(&inner, &client_id) {
                    continue;
                }
                let _ = try_enqueue_pending_request(
                    &inner,
                    PendingRemoteRequest {
                        client_id: client_id.clone(),
                        git_authority: None,
                        action,
                        response: None,
                    },
                );
            }
            Ok(Some(ClientMessage::TakeControl)) => {
                set_native_controller(&inner, Some(client_id.clone()));
            }
            Ok(Some(ClientMessage::ReleaseControl)) => {
                if let Ok(mut controller) = inner.controller_client_id.write() {
                    if controller.as_deref() == Some(client_id.as_str()) {
                        *controller = None;
                    }
                }
            }
            Ok(Some(ClientMessage::Ping)) => {
                if write_message_nonblocking_until_deadline(
                    &mut stream,
                    &ServerMessage::Pong,
                    Instant::now() + REMOTE_CALLBACK_TIMEOUT,
                )
                .is_err()
                {
                    break;
                }
            }
            Ok(Some(ClientMessage::TerminalInput {
                input,
                enqueued_at_epoch_ms,
            })) => {
                if current_controller_allows(&inner, &client_id) {
                    let handler = inner
                        .terminal_input_handler
                        .read()
                        .ok()
                        .and_then(|slot| slot.clone());
                    if let Some(handler) = handler {
                        let _ = run_bounded_remote_callback(
                            &inner,
                            native_runtime_generation,
                            "remote-terminal-input-callback",
                            move || handler(input, enqueued_at_epoch_ms),
                        );
                    }
                }
            }
            Ok(Some(ClientMessage::ResizeSession {
                session_id,
                dimensions,
            })) => {
                if current_controller_allows(&inner, &client_id) {
                    let handler = inner
                        .terminal_resize_handler
                        .read()
                        .ok()
                        .and_then(|slot| slot.clone());
                    if let Some(handler) = handler {
                        let _ = run_bounded_remote_callback(
                            &inner,
                            native_runtime_generation,
                            "remote-terminal-resize-callback",
                            move || handler(session_id, dimensions),
                        );
                    }
                }
            }
            Ok(Some(ClientMessage::Request { request_id, action })) => {
                if requires_control(&action) && !current_controller_allows(&inner, &client_id) {
                    let _ = tx.send(ServerMessage::Response {
                        request_id,
                        result: RemoteActionResult::error(
                            "This client is in viewer mode. Take control first.",
                        ),
                    });
                    continue;
                }

                let timeout = request_timeout_for_action(&action);
                let (response_tx, response_rx) = mpsc::channel();
                if try_enqueue_pending_request(
                    &inner,
                    PendingRemoteRequest {
                        client_id: client_id.clone(),
                        git_authority: None,
                        action,
                        response: Some(response_tx),
                    },
                )
                .is_err()
                {
                    let _ = tx.send(ServerMessage::Response {
                        request_id,
                        result: RemoteActionResult::error("Remote host is busy. Retry shortly."),
                    });
                    continue;
                }
                let result = response_rx
                    .recv_timeout(timeout)
                    .unwrap_or_else(|_| RemoteActionResult::error("Remote host timed out."));
                let _ = tx.send(ServerMessage::Response { request_id, result });
            }
            Ok(Some(ClientMessage::Disconnect)) => break,
            Ok(Some(ClientMessage::Hello { .. } | ClientMessage::PortForwardHello { .. })) => break,
            Ok(None) => match rx.try_recv() {
                Ok(message) => {
                    let is_disconnect = matches!(message, ServerMessage::Disconnected { .. });
                    if write_message_nonblocking_until_deadline(
                        &mut stream,
                        &message,
                        Instant::now() + REMOTE_CALLBACK_TIMEOUT,
                    )
                    .is_err()
                        || is_disconnect
                    {
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    drop(inner);
                    if wait_for_remote_socket_io(
                        &stream.sock,
                        Instant::now() + NATIVE_OUTBOUND_POLL_INTERVAL,
                        false,
                    )
                    .is_err()
                    {
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => break,
            },
            Err(_) => break,
        }
    }

    let Some(inner) = inner_weak.upgrade() else {
        return;
    };
    let _ = stream.sock.shutdown(Shutdown::Both);
    let _removed = remove_exact_native_registration(&inner, connection_id, &native_sender);
    if _removed {
        notify_broadcaster(&inner);
    }
    if _removed {
        if let Ok(mut controller) = inner.controller_client_id.write() {
            if controller.as_deref() == Some(client_id.as_str()) {
                *controller = None;
            }
        }
    }
    set_last_connection_note(
        &inner,
        format!("Remote client {client_id} disconnected from {peer_label}."),
        false,
    );
    // The successful handshake already persisted a last-seen value. Refresh it
    // for an ordinary client disconnect, but do not begin filesystem work after
    // the host generation has been cancelled; root teardown must remain
    // cooperative and bounded under a slow profile filesystem.
    if _removed && !native_connection_should_stop(&inner, native_runtime_generation) {
        if let Err(error) = mutate_host_config_if(
            &inner,
            |config| {
                config
                    .paired_clients
                    .iter()
                    .any(|client| client.client_id == client_id)
            },
            |config| {
                config
                    .paired_clients
                    .iter_mut()
                    .find(|client| client.client_id == client_id)
                    .expect("serialized native client condition must remain true")
                    .last_seen_epoch_ms = Some(now_epoch_ms());
            },
        ) {
            set_last_connection_note(
                &inner,
                format!(
                    "Remote client {client_id} disconnected, but its last-seen update could not be saved: {error}"
                ),
                true,
            );
        }
    }
    #[cfg(test)]
    if _removed {
        notify_native_lifecycle(&inner, NativeLifecycleTestEvent::ClientRemoved);
    }
}

enum PreparedNativeClientAuth {
    PairToken {
        pairing_token: String,
        client_id: String,
        client_token: String,
    },
    ClientToken {
        client_id: String,
        auth_token: String,
    },
}

struct PreparedNativeClientAuthentication {
    auth: PreparedNativeClientAuth,
    client_label: String,
    record_activity: bool,
    activity_ip_address: Option<String>,
}

impl PreparedNativeClientAuthentication {
    fn client_id(&self) -> &str {
        match &self.auth {
            PreparedNativeClientAuth::PairToken { client_id, .. }
            | PreparedNativeClientAuth::ClientToken { client_id, .. } => client_id,
        }
    }

    fn matches(&self, config: &RemoteHostConfig) -> bool {
        match &self.auth {
            PreparedNativeClientAuth::PairToken { pairing_token, .. } => {
                pairing_token.trim() == config.pairing_token.trim()
            }
            PreparedNativeClientAuth::ClientToken {
                client_id,
                auth_token,
            } => config
                .paired_clients
                .iter()
                .any(|client| client.client_id == *client_id && client.auth_token == *auth_token),
        }
    }

    fn rejection_message(&self) -> String {
        match &self.auth {
            PreparedNativeClientAuth::PairToken { .. } => {
                "Pairing token did not match the host.".to_string()
            }
            PreparedNativeClientAuth::ClientToken { .. } => {
                "Saved remote credentials are no longer valid.".to_string()
            }
        }
    }

    fn apply_at(
        &self,
        config: &mut RemoteHostConfig,
        occurred_at_epoch_ms: u64,
    ) -> (String, String, String) {
        match &self.auth {
            PreparedNativeClientAuth::PairToken {
                client_id,
                client_token,
                ..
            } => {
                config.paired_clients.push(PairedRemoteClient {
                    client_id: client_id.clone(),
                    label: self.client_label.clone(),
                    auth_token: client_token.clone(),
                    last_seen_epoch_ms: Some(occurred_at_epoch_ms),
                });
                if self.record_activity {
                    append_native_connection_activity(
                        config,
                        client_id.clone(),
                        self.client_label.clone(),
                        self.activity_ip_address.clone(),
                        occurred_at_epoch_ms,
                    );
                }
                (
                    client_id.clone(),
                    client_token.clone(),
                    self.client_label.clone(),
                )
            }
            PreparedNativeClientAuth::ClientToken {
                client_id,
                auth_token,
            } => {
                let authenticated = {
                    let client = config
                        .paired_clients
                        .iter_mut()
                        .find(|client| {
                            client.client_id == *client_id && client.auth_token == *auth_token
                        })
                        .expect("serialized native client validation must remain true");
                    client.label = self.client_label.clone();
                    client.last_seen_epoch_ms = Some(occurred_at_epoch_ms);
                    (
                        client.client_id.clone(),
                        client.auth_token.clone(),
                        client.label.clone(),
                    )
                };
                if self.record_activity {
                    append_native_connection_activity(
                        config,
                        authenticated.0.clone(),
                        authenticated.2.clone(),
                        self.activity_ip_address.clone(),
                        occurred_at_epoch_ms,
                    );
                }
                authenticated
            }
        }
    }
}

fn prepare_native_client_authentication(
    hello: ClientMessage,
    activity_ip_address: Option<Option<String>>,
) -> Result<PreparedNativeClientAuthentication, String> {
    let ClientMessage::Hello {
        protocol_version,
        client_label,
        auth,
    } = hello
    else {
        return Err("Expected handshake.".to_string());
    };

    if protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "Protocol mismatch. Host uses {}, client uses {protocol_version}.",
            PROTOCOL_VERSION
        ));
    }
    let client_label = client_label.trim().to_string();
    let client_label = if client_label.is_empty() {
        "Desktop app".to_string()
    } else {
        client_label
    };
    let record_activity = activity_ip_address.is_some();
    let activity_ip_address = activity_ip_address.flatten();

    let auth = match auth {
        ClientAuth::PairToken { token } => PreparedNativeClientAuth::PairToken {
            pairing_token: token,
            client_id: generate_secret("client"),
            client_token: generate_secret("auth"),
        },
        ClientAuth::ClientToken {
            client_id,
            auth_token,
        } => PreparedNativeClientAuth::ClientToken {
            client_id,
            auth_token,
        },
    };
    Ok(PreparedNativeClientAuthentication {
        auth,
        client_label,
        record_activity,
        activity_ip_address,
    })
}

fn validate_prepared_native_authentication(
    inner: &Arc<RemoteHostInner>,
    authentication: &PreparedNativeClientAuthentication,
) -> Result<(), String> {
    let config = inner
        .config
        .read()
        .map_err(|_| "host config unavailable".to_string())?;
    authentication
        .matches(&config)
        .then_some(())
        .ok_or_else(|| authentication.rejection_message())
}

fn admit_native_client(
    inner: &Arc<RemoteHostInner>,
    native_runtime_generation: u64,
    connection_id: u64,
    authentication: &PreparedNativeClientAuthentication,
    client: ConnectedRemoteClient,
) -> Result<Option<(String, String, String)>, HostConfigAdmissionError> {
    // The host-config transaction is always first. Lifecycle is held only for
    // short generation/auth fences; persistence and compensation run with
    // neither lifecycle nor config-memory locks held. Phase A persists only a
    // non-success attempt marker. Connected/auth state is written only after
    // the explicit Phase-B admission fence.
    let _transaction = inner.host_config_tx.lock().map_err(|_| {
        HostConfigAdmissionError::Persistence("host config transaction unavailable".to_string())
    })?;
    let attempt_id = inner
        .next_host_config_attempt_id
        .fetch_add(1, Ordering::Relaxed);
    let attempt_nonce = generate_secret("admission");
    let pending = {
        let _lifecycle = inner
            .lifecycle_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if native_connection_should_stop(inner, native_runtime_generation) {
            return Ok(None);
        }
        validate_prepared_native_authentication(inner, authentication)
            .map_err(HostConfigAdmissionError::Persistence)?;
        if inner
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&connection_id)
        {
            return Err(HostConfigAdmissionError::Persistence(
                "Remote connection identity is already registered.".to_string(),
            ));
        }
        let pending_attempt = PendingRemoteAdmissionAttempt {
            attempt_nonce: attempt_nonce.clone(),
            source: RemoteAccessSource::NativeApp,
            client_id: authentication.client_id().to_string(),
            generation: native_runtime_generation,
            attempted_at_epoch_ms: now_epoch_ms(),
        };
        stage_host_config_mutation(inner, move |config| {
            append_pending_admission_attempt(config, pending_attempt)
        })
        .map_err(HostConfigAdmissionError::Persistence)?
    };

    persist_host_config_snapshot(&pending.candidate)
        .map_err(|error| HostConfigAdmissionError::Persistence(error.to_string()))?;

    let final_staged = {
        let _lifecycle = inner
            .lifecycle_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (current_matches, auth_is_current) = inner
            .config
            .read()
            .map(|config| {
                (
                    inner.config_revision.load(Ordering::Acquire) == pending.base_revision
                        && *config == pending.base,
                    authentication.matches(&config),
                )
            })
            .unwrap_or((false, false));
        let identity_available = !inner
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&connection_id);
        if native_connection_should_stop(inner, native_runtime_generation)
            || !current_matches
            || !auth_is_current
            || !identity_available
        {
            None
        } else {
            let mut candidate = pending.candidate.clone();
            if !remove_pending_admission_attempt(&mut candidate, &attempt_nonce) {
                return Err(HostConfigAdmissionError::Persistence(
                    "Native admission attempt marker disappeared before Phase B.".to_string(),
                ));
            }
            // `last_seen` and Connected/Reconnected describe this Phase-B
            // authorization fence. They intentionally do not reuse the
            // earlier durable attempt-marker timestamp.
            let result = authentication.apply_at(&mut candidate, now_epoch_ms());
            Some(StagedHostConfigMutation {
                base_revision: pending.base_revision,
                base: pending.base.clone(),
                candidate,
                result,
            })
        }
    };
    let Some(final_staged) = final_staged else {
        compensate_rejected_host_config_admission(&pending, attempt_id)?;
        return Ok(None);
    };

    if let Err(error) = persist_host_config_snapshot(&final_staged.candidate) {
        compensate_rejected_host_config_candidates(
            &[&final_staged.candidate, &pending.candidate],
            &pending.base,
            attempt_id,
        )?;
        return Err(HostConfigAdmissionError::Persistence(error.to_string()));
    }

    let accepted = {
        let _lifecycle = inner
            .lifecycle_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (current_matches, auth_is_current) = inner
            .config
            .read()
            .map(|config| {
                (
                    inner.config_revision.load(Ordering::Acquire) == final_staged.base_revision
                        && *config == final_staged.base,
                    authentication.matches(&config),
                )
            })
            .unwrap_or((false, false));
        let identity_available = !inner
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&connection_id);
        if native_connection_should_stop(inner, native_runtime_generation)
            || !current_matches
            || !auth_is_current
            || !identity_available
        {
            false
        } else {
            *inner
                .config
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = final_staged.candidate.clone();
            inner
                .clients
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(connection_id, client);
            bump_host_config_revision(inner);
            true
        }
    };
    if !accepted {
        compensate_rejected_host_config_candidates(
            &[&final_staged.candidate, &pending.candidate],
            &pending.base,
            attempt_id,
        )?;
        return Ok(None);
    }
    Ok(Some(final_staged.result))
}

#[cfg(test)]
fn authenticate_client(
    inner: &Arc<RemoteHostInner>,
    hello: ClientMessage,
) -> Result<(String, String, String), String> {
    authenticate_client_with_activity(inner, hello, None)
}

#[cfg(test)]
fn authenticate_client_with_activity(
    inner: &Arc<RemoteHostInner>,
    hello: ClientMessage,
    activity_ip_address: Option<Option<String>>,
) -> Result<(String, String, String), String> {
    let authentication = prepare_native_client_authentication(hello, activity_ip_address)?;
    mutate_host_config_if(
        inner,
        |config| authentication.matches(config),
        |config| authentication.apply_at(config, now_epoch_ms()),
    )
    .map_err(|error| error.to_string())?
    .ok_or_else(|| authentication.rejection_message())
}

fn handle_port_forward_connection(
    inner: &Arc<RemoteHostInner>,
    peer_label: &str,
    stream: &mut transport::ServerTlsStream,
    hello: ClientMessage,
    native_runtime_generation: u64,
    handshake_deadline: Instant,
    cancellation: &Arc<ForwardCancellation>,
) -> Result<(), String> {
    let (client_id, auth_token, requested_port) = authenticate_port_forward(inner, hello)?;
    let mut last_connect_error = None;
    let mut upstream = None;
    #[cfg(test)]
    if let Some(connector) = inner
        .port_forward_connector_test_hook
        .read()
        .ok()
        .and_then(|hook| hook.clone())
    {
        upstream = Some(connector(requested_port)?);
    }
    if upstream.is_none() {
        for address in [
            SocketAddr::from((Ipv4Addr::LOCALHOST, requested_port)),
            SocketAddr::from((Ipv6Addr::LOCALHOST, requested_port)),
        ] {
            if cancellation.is_cancelled()
                || native_connection_should_stop(inner, native_runtime_generation)
            {
                return Err("Remote host stopped before the port forward connected.".to_string());
            }
            let remaining = handshake_deadline
                .saturating_duration_since(Instant::now())
                .min(PORT_FORWARD_CONNECT_TIMEOUT);
            if remaining.is_zero() {
                return Err(
                    "Remote port-forward handshake deadline expired before upstream connect."
                        .to_string(),
                );
            }
            match TcpStream::connect_timeout(&address, remaining) {
                Ok(stream) => {
                    upstream = Some(stream);
                    break;
                }
                Err(error) => last_connect_error = Some(error),
            }
            if cancellation.is_cancelled() {
                return Err("Remote host stopped while the port forward connected.".to_string());
            }
        }
    }
    let mut upstream = upstream.ok_or_else(|| {
        format!(
            "Could not connect to host localhost:{requested_port}: {}",
            last_connect_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "no loopback address was available".to_string())
        )
    })?;
    if !cancellation.register(&upstream) {
        let _ = upstream.shutdown(Shutdown::Both);
        return Err("Remote host stopped while the port forward connected.".to_string());
    }
    let _ = upstream.set_nodelay(true);
    let _ = upstream.set_read_timeout(None);
    let _ = upstream.set_write_timeout(None);
    set_server_handshake_write_deadline(stream, handshake_deadline)?;
    write_message_until_deadline(stream, &ServerMessage::PortForwardOk, handshake_deadline)
        .map_err(|error| format!("Could not start port forward: {error}"))?;
    if let Err(error) = copy_bidirectional(&mut upstream, stream, cancellation, || {
        native_connection_should_stop(inner, native_runtime_generation)
            || !native_client_credentials_are_current(inner, &client_id, &auth_token)
    }) {
        eprintln!(
            "[remote] port forward {requested_port} for {client_id} from {peer_label} ended with error: {error}"
        );
    }
    let _ = upstream.shutdown(Shutdown::Both);
    let _ = stream.sock.shutdown(Shutdown::Both);
    Ok(())
}

fn authenticate_port_forward(
    inner: &Arc<RemoteHostInner>,
    hello: ClientMessage,
) -> Result<(String, String, u16), String> {
    let ClientMessage::PortForwardHello {
        protocol_version,
        server_id,
        client_id,
        auth_token,
        requested_port,
    } = hello
    else {
        return Err("Expected a port-forward handshake.".to_string());
    };

    if protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "Protocol mismatch. Host uses {}, client uses {protocol_version}.",
            PROTOCOL_VERSION
        ));
    }
    {
        let config = inner
            .config
            .read()
            .map_err(|_| "Remote host credentials are temporarily unavailable.".to_string())?;
        if server_id != config.server_id {
            return Err("This client targeted a different host identity.".to_string());
        }
        if !config
            .paired_clients
            .iter()
            .any(|client| client.client_id == client_id && client.auth_token == auth_token)
        {
            return Err("Saved remote credentials are no longer valid.".to_string());
        }
    }
    if !host_can_forward_port(inner, requested_port) {
        return Err(format!(
            "Port {requested_port} is not a live DevManager server port on this host."
        ));
    }
    Ok((client_id, auth_token, requested_port))
}

fn native_client_credentials_are_current(
    inner: &RemoteHostInner,
    client_id: &str,
    auth_token: &str,
) -> bool {
    inner.config.read().is_ok_and(|config| {
        config
            .paired_clients
            .iter()
            .any(|client| client.client_id == client_id && client.auth_token == auth_token)
    })
}

fn host_can_forward_port(inner: &Arc<RemoteHostInner>, requested_port: u16) -> bool {
    let _snapshot_guard = inner
        .snapshot_state_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let app_state = inner
        .shared_state
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let runtime_state = inner
        .runtime_state
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let now_epoch_ms = now_epoch_ms();
    let raw_port_authorities = inner
        .port_authorities
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let managed_port_snapshots = inner
        .managed_port_snapshots
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let observation_time = Instant::now();
    let deadline = observation_time
        .checked_add(crate::process::ports::DEFAULT_MEMBERSHIP_MAX_AGE)
        .unwrap_or(observation_time);
    let port_authorities = host_verified_port_authorities_at(
        &raw_port_authorities,
        &runtime_state,
        &managed_port_snapshots,
        now_epoch_ms,
        observation_time,
        deadline,
    );
    for project in app_state.projects() {
        for folder in &project.folders {
            for command in &folder.commands {
                if command.port != Some(requested_port) {
                    continue;
                }
                let Some(session) = runtime_state.sessions.get(&command.id) else {
                    continue;
                };
                let Some(authority) = port_authorities.get(&requested_port) else {
                    continue;
                };
                if session.status.is_live()
                    && remote_authority_allows_forward_with_live_at(
                        authority,
                        requested_port,
                        session,
                        now_epoch_ms,
                        managed_port_snapshots
                            .get(&requested_port)
                            .map(|capability| capability.as_ref()),
                        observation_time,
                        deadline,
                    )
                {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
fn remote_authority_allows_forward(
    authority: &RemotePortAuthority,
    requested_port: u16,
    session: &SessionRuntimeState,
    now_epoch_ms: u64,
) -> bool {
    remote_authority_allows_forward_with_live(
        authority,
        requested_port,
        session,
        now_epoch_ms,
        None,
    )
}

#[cfg(test)]
fn remote_authority_allows_forward_with_live(
    authority: &RemotePortAuthority,
    requested_port: u16,
    session: &SessionRuntimeState,
    now_epoch_ms: u64,
    live: Option<&ManagedResourceCapability>,
) -> bool {
    let observation_time = Instant::now();
    let deadline = observation_time
        .checked_add(crate::process::ports::DEFAULT_MEMBERSHIP_MAX_AGE)
        .unwrap_or(observation_time);
    remote_authority_allows_forward_with_live_at(
        authority,
        requested_port,
        session,
        now_epoch_ms,
        live,
        observation_time,
        deadline,
    )
}

fn remote_authority_allows_forward_with_live_at(
    authority: &RemotePortAuthority,
    requested_port: u16,
    session: &SessionRuntimeState,
    now_epoch_ms: u64,
    live: Option<&ManagedResourceCapability>,
    observation_time: Instant,
    deadline: Instant,
) -> bool {
    authority.kind() == RemotePortAuthorityKind::Managed
        && authority.is_fresh_at(now_epoch_ms)
        && live.is_some_and(|live| {
            authority.has_exact_managed_fence_for(
                requested_port,
                session,
                live,
                now_epoch_ms,
                observation_time,
                deadline,
            )
        })
}

fn bump_host_config_revision(inner: &Arc<RemoteHostInner>) {
    let previous = inner.config_revision.fetch_add(1, Ordering::Relaxed);
    let revision = previous.wrapping_add(1);
    // send_replace keeps the watch current even when no receivers are attached.
    inner.host_config_watch.send_replace(revision);
}

pub(crate) fn browser_admission_now_epoch_ms(inner: &Arc<RemoteHostInner>) -> u64 {
    #[cfg(test)]
    if let Some(clock) = inner
        .browser_admission_clock_test_hook
        .read()
        .ok()
        .and_then(|clock| clock.clone())
    {
        return clock();
    }
    now_epoch_ms()
}

pub(crate) fn surface_connect_startup(
    inner: &Arc<RemoteHostInner>,
    error: Option<String>,
    is_error: bool,
) {
    if let Ok(mut slot) = inner.connect_startup_error.write() {
        *slot = error.clone();
    }
    if is_error {
        if let Some(note) = error {
            set_last_connection_note(inner, format!("Connect production: {note}"), true);
        }
    }
}

pub(crate) fn set_last_connection_note(inner: &Arc<RemoteHostInner>, note: String, is_error: bool) {
    if let Ok(mut slot) = inner.last_connection_note.write() {
        *slot = Some(note);
    }
    inner
        .last_connection_is_error
        .store(is_error, Ordering::Relaxed);
}

pub(crate) fn publish_semantic_event(
    inner: &Arc<RemoteHostInner>,
    draft: SemanticEventDraft,
) -> SemanticEvent {
    let service = RemoteHostService::borrowed(inner.clone());
    let mut published = None;
    service.publish_semantic_change(|journals| {
        published = Some(journals.record(draft));
        true
    });
    published.expect("semantic event publication completed without an event")
}

fn deferred_claude_hook(pending: PendingClaudeComposerPrompt) -> Option<SemanticEventDraft> {
    match pending.state {
        PendingClaudeComposerPromptState::Reserved { deferred_hook } => deferred_hook,
        PendingClaudeComposerPromptState::Accepted => None,
    }
}

fn remove_pending_claude_prompts(
    state: &mut ClaudeComposerReconciliationState,
    mut predicate: impl FnMut(&PendingClaudeComposerPrompt) -> bool,
) -> Vec<SemanticEventDraft> {
    let mut deferred = Vec::new();
    let mut index = 0;
    while index < state.pending.len() {
        if predicate(&state.pending[index]) {
            if let Some(draft) = state.pending.remove(index).and_then(deferred_claude_hook) {
                deferred.push(draft);
            }
        } else {
            index += 1;
        }
    }
    deferred
}

fn drain_expired_claude_reconciliations(
    state: &mut ClaudeComposerReconciliationState,
    now: Instant,
) -> Vec<SemanticEventDraft> {
    state
        .reconciled_provider_keys
        .retain(|entry| now <= entry.expires_at);
    remove_pending_claude_prompts(state, |pending| now > pending.expires_at)
}

fn remember_reconciled_claude_provider_key(
    state: &mut ClaudeComposerReconciliationState,
    identity: ClaudeSemanticIdentity,
    key: String,
    now: Instant,
) {
    state
        .reconciled_provider_keys
        .retain(|entry| entry.identity != identity || entry.key != key);
    while state.reconciled_provider_keys.len() >= MAX_CLAUDE_COMPOSER_RECONCILIATIONS {
        state.reconciled_provider_keys.pop_front();
    }
    state
        .reconciled_provider_keys
        .push_back(ReconciledClaudeProviderKey {
            identity,
            key,
            expires_at: now + CLAUDE_COMPOSER_RECONCILIATION_TTL,
        });
}

fn deferred_codex_provider(pending: PendingCodexComposerPrompt) -> Option<SemanticEventDraft> {
    match pending.state {
        PendingCodexComposerPromptState::Reserved { deferred_provider } => deferred_provider,
        PendingCodexComposerPromptState::Accepted => None,
    }
}

fn remove_pending_codex_prompts(
    state: &mut CodexComposerReconciliationState,
    mut predicate: impl FnMut(&PendingCodexComposerPrompt) -> bool,
) -> Vec<SemanticEventDraft> {
    let mut deferred = Vec::new();
    let mut index = 0;
    while index < state.pending.len() {
        if predicate(&state.pending[index]) {
            if let Some(draft) = state
                .pending
                .remove(index)
                .and_then(deferred_codex_provider)
            {
                deferred.push(draft);
            }
        } else {
            index += 1;
        }
    }
    deferred
}

fn drain_expired_codex_reconciliations(
    state: &mut CodexComposerReconciliationState,
    now: Instant,
) -> Vec<SemanticEventDraft> {
    state
        .reconciled_provider_keys
        .retain(|entry| now <= entry.expires_at);
    remove_pending_codex_prompts(state, |pending| now > pending.expires_at)
}

fn remember_reconciled_codex_provider_key(
    state: &mut CodexComposerReconciliationState,
    identity: CodexSemanticIdentity,
    key: String,
    now: Instant,
) {
    state
        .reconciled_provider_keys
        .retain(|entry| entry.identity != identity || entry.key != key);
    while state.reconciled_provider_keys.len() >= MAX_CODEX_COMPOSER_RECONCILIATIONS {
        state.reconciled_provider_keys.pop_front();
    }
    state
        .reconciled_provider_keys
        .push_back(ReconciledCodexProviderKey {
            identity,
            key,
            expires_at: now + CODEX_COMPOSER_RECONCILIATION_TTL,
        });
}

/// Fan semantic journal changes out through the bounded browser-only channel.
/// A delivery-only lock orders this against subscribe/unsubscribe without ever
/// excluding PTY publication. No client lock is nested with the journal lock,
/// and `try_send` never waits. Saturated clients are disconnected and recover
/// by replaying from their last acknowledged cursor after reconnect.
fn deliver_live_semantic_events(inner: &Arc<RemoteHostInner>) -> bool {
    let delivery = inner
        .semantic_delivery_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    #[cfg(test)]
    {
        let hook = inner
            .semantic_delivery_test_hook
            .read()
            .ok()
            .and_then(|hook| hook.clone());
        if let Some(hook) = hook {
            hook();
        }
    }
    let subscriptions = inner
        .clients
        .lock()
        .map(|clients| {
            clients
                .iter()
                .filter_map(|(connection_id, client)| {
                    let sender = client.web_sender.clone()?;
                    let tombstone = client.web_tombstone.clone()?;
                    Some(
                        client
                            .semantic_cursors
                            .iter()
                            .map(|(key, cursor)| {
                                (
                                    *connection_id,
                                    client.client_id.clone(),
                                    sender.clone(),
                                    tombstone.clone(),
                                    key.clone(),
                                    *cursor,
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .flatten()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut dead_connections = Vec::new();
    for (connection_id, client_id, sender, tombstone, key, cursor) in subscriptions {
        let capture = inner
            .semantic_journals
            .lock()
            .ok()
            .and_then(|journals| journals.capture_replay_after(&key, cursor));
        let Some(capture) = capture else {
            continue;
        };
        let replay = capture.into_replay();
        let through_sequence = replay.through_sequence;
        if through_sequence == cursor {
            continue;
        }
        if replay.cursor_rolled_over {
            dead_connections.push((
                connection_id,
                client_id,
                tombstone,
                Some("Semantic history rolled over. Reconnecting for a clean resume.".to_string()),
            ));
            continue;
        }
        let send_result = sender.try_send_live_events(&replay.events);
        if send_result.is_err() {
            dead_connections.push((connection_id, client_id, tombstone, None));
            continue;
        }
        if let Ok(mut clients) = inner.clients.lock() {
            let Some(client) = clients.get_mut(&connection_id).filter(|client| {
                client.client_id == client_id
                    && client.semantic_cursors.get(&key) == Some(&cursor)
                    && client
                        .web_tombstone
                        .as_ref()
                        .is_some_and(|registered| Arc::ptr_eq(registered, &tombstone))
            }) else {
                continue;
            };
            client.semantic_cursors.insert(key, through_sequence);
        }
    }
    drop(delivery);
    dead_connections.sort_unstable_by_key(|(connection_id, _, _, _)| *connection_id);
    let mut deduplicated: Vec<(
        u64,
        String,
        Arc<web::bridge::WebConnectionTombstone>,
        Option<String>,
    )> = Vec::new();
    for dead in dead_connections {
        if let Some(previous) = deduplicated
            .last_mut()
            .filter(|(connection_id, _, _, _)| *connection_id == dead.0)
        {
            if previous.3.is_none() {
                previous.3 = dead.3;
            }
        } else {
            deduplicated.push(dead);
        }
    }
    for (connection_id, client_id, tombstone, reason) in deduplicated {
        web::bridge::revoke_web_connection(inner, connection_id, &client_id, &tombstone, reason);
    }
    true
}

fn drain_web_clients_for_restart(inner: &Arc<RemoteHostInner>) {
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let connections = inner
        .clients
        .lock()
        .map(|clients| {
            clients
                .iter()
                .filter_map(|(connection_id, client)| {
                    Some((
                        *connection_id,
                        client.client_id.clone(),
                        client.web_tombstone.clone()?,
                    ))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for (connection_id, client_id, tombstone) in connections {
        web::bridge::revoke_web_connection_locked(
            inner,
            connection_id,
            &client_id,
            &tombstone,
            Some("The browser listener is restarting.".to_string()),
        );
    }

    let controller_id = inner
        .controller_client_id
        .read()
        .map(|controller| controller.clone())
        .unwrap_or_default();
    let (request, clear_web_controller) = inner
        .web_control
        .lock()
        .map(|mut control| {
            let web_controller_id = control
                .writer_leases()
                .peek()
                .map(|lease| lease.owner_client_id)
                .or_else(|| control.legacy_claimant_client_id().map(str::to_string));
            let clear_web_controller =
                controller_id.is_some() && controller_id.as_deref() == web_controller_id.as_deref();
            (
                control.reset_web(clear_web_controller),
                clear_web_controller,
            )
        })
        .unwrap_or((ControllerRequest::Deferred, false));
    if matches!(request, ControllerRequest::Applied { .. }) && clear_web_controller {
        if let Ok(mut controller) = inner.controller_client_id.write() {
            if *controller == controller_id {
                *controller = None;
            }
        }
    }
}

pub(crate) fn set_native_controller(
    inner: &Arc<RemoteHostInner>,
    controller_client_id: Option<String>,
) {
    let target = controller_client_id
        .clone()
        .map(ControllerTarget::Native)
        .unwrap_or(ControllerTarget::Local);
    let _operation = inner
        .web_control_operation_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let request = inner
        .web_control
        .lock()
        .map(|mut control| control.request_controller(target))
        .unwrap_or(ControllerRequest::Deferred);
    if matches!(request, ControllerRequest::Applied { .. }) {
        if let Ok(mut controller) = inner.controller_client_id.write() {
            *controller = controller_client_id;
        }
    }
    web::bridge::broadcast_writer_lease_state_locked(inner, now_epoch_ms());
}

pub(crate) fn current_controller_allows(inner: &Arc<RemoteHostInner>, client_id: &str) -> bool {
    inner
        .controller_client_id
        .read()
        .ok()
        .and_then(|controller| controller.clone())
        .is_some_and(|controller| controller == client_id)
}

pub(crate) fn requires_control(action: &RemoteAction) -> bool {
    !matches!(
        action,
        RemoteAction::SearchSession { .. }
            | RemoteAction::ScrollSession { .. }
            | RemoteAction::ScrollSessionToBufferLine { .. }
            | RemoteAction::ScrollSessionToOffset { .. }
            | RemoteAction::BrowsePath { .. }
            | RemoteAction::ListDirectory { .. }
            | RemoteAction::StatPath { .. }
            | RemoteAction::ReadTextFile { .. }
            | RemoteAction::ScanFolder { .. }
            | RemoteAction::ScanRoot { .. }
            | RemoteAction::ExportSessionText { .. }
            | RemoteAction::GitListRepos
            | RemoteAction::GitStatus { .. }
            | RemoteAction::GitLog { .. }
            | RemoteAction::GitDiffFile { .. }
            | RemoteAction::GitDiffCommit { .. }
            | RemoteAction::GitBranches { .. }
            | RemoteAction::GitGetGithubAuthStatus
    )
}

pub(crate) fn request_timeout_for_action(action: &RemoteAction) -> Duration {
    match action {
        RemoteAction::LaunchAi { .. }
        | RemoteAction::OpenAiTab { .. }
        | RemoteAction::RestartAiTab { .. } => AI_STARTUP_REQUEST_TIMEOUT,
        RemoteAction::GitCommit { .. }
        | RemoteAction::GitPush { .. }
        | RemoteAction::GitPushSetUpstream { .. }
        | RemoteAction::GitPull { .. }
        | RemoteAction::GitFetch { .. }
        | RemoteAction::GitSync { .. } => GIT_REQUEST_TIMEOUT,
        _ => REQUEST_TIMEOUT,
    }
}

fn apply_remote_session_output(
    inner: &Arc<RemoteClientInner>,
    session_id: &str,
    bytes: &[u8],
) -> bool {
    if bytes.is_empty() {
        return false;
    }

    if let Ok(replicas) = inner.session_replicas.read() {
        if let Some(replica) = replicas.get(session_id) {
            replica.apply_output_bytes(bytes);
            return true;
        }
    }

    let runtime = inner.latest_snapshot.read().ok().and_then(|snapshot| {
        snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.runtime_state.sessions.get(session_id).cloned())
    });
    let Some(runtime) = runtime else {
        return false;
    };

    let replica = TerminalReplica::from_bootstrap(session_id.to_string(), runtime, &[]);
    replica.apply_output_bytes(bytes);

    if let Ok(mut replicas) = inner.session_replicas.write() {
        if let Some(existing) = replicas.get(session_id) {
            existing.apply_output_bytes(bytes);
        } else {
            replicas.insert(session_id.to_string(), replica);
        }
        return true;
    }

    false
}

fn run_client_connection(
    mut stream: transport::ClientTlsStream,
    rx: mpsc::Receiver<ClientMessage>,
    inner: Arc<RemoteClientInner>,
) {
    let mut read_buffer = Vec::new();
    let mut last_heartbeat_at = Instant::now();
    let _ = stream
        .sock
        .set_read_timeout(Some(NATIVE_OUTBOUND_POLL_INTERVAL));

    while inner
        .disconnected_message
        .read()
        .ok()
        .and_then(|message| message.clone())
        .is_none()
    {
        let mut should_break = false;
        for _ in 0..MAX_OUTBOUND_MESSAGES_PER_TICK {
            match rx.try_recv() {
                Ok(message) => {
                    let is_disconnect = matches!(message, ClientMessage::Disconnect);
                    if write_message(&mut stream, &message).is_err() {
                        if let Ok(mut disconnected) = inner.disconnected_message.write() {
                            *disconnected = Some("Remote host connection was lost.".to_string());
                        }
                        should_break = true;
                        break;
                    }
                    if is_disconnect {
                        let _ = stream.sock.shutdown(Shutdown::Both);
                        should_break = true;
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    should_break = true;
                    break;
                }
            }
        }
        if should_break {
            break;
        }

        if last_heartbeat_at.elapsed() >= HEARTBEAT_INTERVAL {
            if write_message(&mut stream, &ClientMessage::Ping).is_err() {
                if let Ok(mut disconnected) = inner.disconnected_message.write() {
                    *disconnected = Some("Remote host connection was lost.".to_string());
                }
                break;
            }
            last_heartbeat_at = Instant::now();
        }

        match try_read_message::<ServerMessage, _>(&mut stream, &mut read_buffer) {
            Ok(Some(ServerMessage::Snapshot { snapshot })) => {
                if let Ok(mut replicas) = inner.session_replicas.write() {
                    replicas.clear();
                }
                if let Ok(mut latest) = inner.latest_snapshot.write() {
                    *latest = Some(snapshot);
                }
                inner.snapshot_revision.fetch_add(1, Ordering::Relaxed);
                inner
                    .session_stream_revision
                    .fetch_add(1, Ordering::Relaxed);
            }
            Ok(Some(ServerMessage::Delta { delta })) => {
                if let Ok(mut latest) = inner.latest_snapshot.write() {
                    let snapshot = latest.get_or_insert_with(RemoteWorkspaceSnapshot::default);
                    apply_workspace_delta(snapshot, delta);
                }
                inner.snapshot_revision.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Some(ServerMessage::SessionStream { event })) => {
                match event {
                    RemoteSessionStreamEvent::Bootstrap { bootstrap } => {
                        let session_id = bootstrap.session_id.clone();
                        if let Ok(mut replicas) = inner.session_replicas.write() {
                            replicas.insert(
                                session_id.clone(),
                                TerminalReplica::from_bootstrap(
                                    bootstrap.session_id.clone(),
                                    bootstrap.runtime.clone(),
                                    &bootstrap.replay_bytes,
                                ),
                            );
                        }
                        if let Ok(mut latest) = inner.latest_snapshot.write() {
                            if let Some(snapshot) = latest.as_mut() {
                                snapshot.session_views.insert(
                                    session_id.clone(),
                                    TerminalSessionView {
                                        runtime: bootstrap.runtime.clone(),
                                        screen: bootstrap.screen.clone(),
                                    },
                                );
                                snapshot
                                    .runtime_state
                                    .sessions
                                    .insert(session_id, bootstrap.runtime);
                            }
                        }
                    }
                    RemoteSessionStreamEvent::Output {
                        session_id,
                        emitted_at_epoch_ms,
                        bytes,
                        ..
                    } => {
                        note_remote_output_received(&inner, emitted_at_epoch_ms);
                        apply_remote_session_output(&inner, &session_id, &bytes);
                    }
                    RemoteSessionStreamEvent::RuntimePatch {
                        session_id,
                        runtime,
                    }
                    | RemoteSessionStreamEvent::Closed {
                        session_id,
                        runtime,
                    } => {
                        let fire_notification = {
                            if let Ok(latest) = inner.latest_snapshot.read() {
                                latest
                                    .as_ref()
                                    .and_then(|s| s.runtime_state.sessions.get(&session_id))
                                    .map(|s| runtime.notification_count > s.notification_count)
                                    .unwrap_or(false)
                            } else {
                                false
                            }
                        };
                        if fire_notification {
                            inner
                                .pending_notification_count
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        if let Ok(replicas) = inner.session_replicas.read() {
                            if let Some(replica) = replicas.get(&session_id) {
                                replica.apply_runtime(runtime.clone());
                            }
                        }
                        if let Ok(mut latest) = inner.latest_snapshot.write() {
                            if let Some(snapshot) = latest.as_mut() {
                                if let Some(view) = snapshot.session_views.get_mut(&session_id) {
                                    view.runtime = runtime.clone();
                                    sync_screen_snapshot_dimensions(
                                        &mut view.screen,
                                        runtime.dimensions,
                                    );
                                }
                                snapshot
                                    .runtime_state
                                    .sessions
                                    .insert(session_id.clone(), runtime);
                            }
                        }
                    }
                    RemoteSessionStreamEvent::Removed { session_id } => {
                        if let Ok(mut replicas) = inner.session_replicas.write() {
                            replicas.remove(&session_id);
                        }
                        if let Ok(mut latest) = inner.latest_snapshot.write() {
                            if let Some(snapshot) = latest.as_mut() {
                                snapshot.session_views.remove(&session_id);
                                snapshot.runtime_state.sessions.remove(&session_id);
                            }
                        }
                    }
                }
                inner
                    .session_stream_revision
                    .fetch_add(1, Ordering::Relaxed);
            }
            Ok(Some(ServerMessage::Response { request_id, result })) => {
                if let Ok(mut pending) = inner.pending.lock() {
                    if let Some(sender) = pending.remove(&request_id) {
                        let _ = sender.send(result);
                    }
                }
            }
            Ok(Some(ServerMessage::Disconnected { message })) => {
                if let Ok(mut disconnected) = inner.disconnected_message.write() {
                    *disconnected = Some(message);
                }
                break;
            }
            Ok(Some(
                ServerMessage::HelloOk { .. }
                | ServerMessage::PortForwardOk
                | ServerMessage::HelloErr { .. }
                | ServerMessage::Error { .. }
                | ServerMessage::Pong,
            )) => {}
            Ok(None) => match rx.try_recv() {
                Ok(message) => {
                    let is_disconnect = matches!(message, ClientMessage::Disconnect);
                    if write_message(&mut stream, &message).is_err() || is_disconnect {
                        let _ = stream.sock.shutdown(Shutdown::Both);
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => break,
            },
            Err(_) => {
                if let Ok(mut disconnected) = inner.disconnected_message.write() {
                    *disconnected = Some("Remote host connection was lost.".to_string());
                }
                break;
            }
        }
    }

    if let Ok(mut disconnected) = inner.disconnected_message.write() {
        if disconnected.is_none() {
            *disconnected = Some("Remote host connection was lost.".to_string());
        }
    }
    #[cfg(test)]
    if let Some(hook) = inner
        .reader_exit_test_hook
        .read()
        .ok()
        .and_then(|slot| slot.clone())
    {
        hook();
    }
}

fn write_message<T: Serialize, W: Write>(stream: &mut W, message: &T) -> Result<(), String> {
    let payload = to_vec_named(message).map_err(|error| format!("Serialize failed: {error}"))?;
    let len = payload.len() as u32;
    stream
        .write_all(&len.to_be_bytes())
        .map_err(|error| format!("Write failed: {error}"))?;
    stream
        .write_all(&payload)
        .map_err(|error| format!("Write failed: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("Write failed: {error}"))
}

fn write_message_until_deadline<T: Serialize, W: Write>(
    stream: &mut W,
    message: &T,
    deadline: Instant,
) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err("Remote handshake write deadline expired.".to_string());
    }
    write_message(stream, message)
}

fn write_client_message_until_deadline_cancelled<T: Serialize>(
    stream: &mut transport::ClientTlsStream,
    message: &T,
    deadline: Instant,
    cancellation: &ForwardCancellation,
) -> Result<(), String> {
    let payload = to_vec_named(message).map_err(|error| format!("Serialize failed: {error}"))?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    let mut written = 0;
    while written < frame.len() {
        if cancellation.is_cancelled() {
            return Err("Remote handshake write cancelled.".to_string());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Remote handshake write deadline expired.".to_string());
        }
        stream
            .sock
            .set_write_timeout(Some(remaining.min(Duration::from_millis(50))))
            .map_err(|error| format!("Failed to configure handshake write timeout: {error}"))?;
        match stream.write(&frame[written..]) {
            Ok(0) => return Err("Remote connection closed during handshake write.".to_string()),
            Ok(bytes) => written += bytes,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::Interrupted | ErrorKind::TimedOut | ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(format!("Write failed: {error}")),
        }
    }
    loop {
        if cancellation.is_cancelled() {
            return Err("Remote handshake flush cancelled.".to_string());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Remote handshake flush deadline expired.".to_string());
        }
        stream
            .sock
            .set_write_timeout(Some(remaining.min(Duration::from_millis(50))))
            .map_err(|error| format!("Failed to configure handshake flush timeout: {error}"))?;
        match stream.flush() {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::Interrupted | ErrorKind::TimedOut | ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Err(format!("Write failed: {error}")),
        }
    }
}

fn write_message_nonblocking_until_deadline<T: Serialize>(
    stream: &mut transport::ServerTlsStream,
    message: &T,
    deadline: Instant,
) -> Result<(), String> {
    let payload = to_vec_named(message).map_err(|error| format!("Serialize failed: {error}"))?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    let mut written = 0;
    while written < frame.len() {
        if Instant::now() >= deadline {
            return Err("Remote nonblocking write deadline expired.".to_string());
        }
        match stream.write(&frame[written..]) {
            Ok(0) => return Err("Remote connection closed during write.".to_string()),
            Ok(bytes) => written += bytes,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                if !wait_for_remote_socket_io(&stream.sock, deadline, true)
                    .map_err(|error| format!("Remote write readiness failed: {error}"))?
                {
                    return Err("Remote nonblocking write deadline expired.".to_string());
                }
            }
            Err(error) => return Err(format!("Write failed: {error}")),
        }
    }
    loop {
        if Instant::now() >= deadline {
            return Err("Remote nonblocking flush deadline expired.".to_string());
        }
        match stream.flush() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                if !wait_for_remote_socket_io(&stream.sock, deadline, true)
                    .map_err(|error| format!("Remote flush readiness failed: {error}"))?
                {
                    return Err("Remote nonblocking flush deadline expired.".to_string());
                }
            }
            Err(error) => return Err(format!("Write failed: {error}")),
        }
    }
}

fn set_server_handshake_write_deadline(
    stream: &mut transport::ServerTlsStream,
    deadline: Instant,
) -> Result<(), String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("Remote handshake write deadline expired.".to_string());
    }
    stream
        .sock
        .set_write_timeout(Some(remaining.min(Duration::from_secs(5))))
        .map_err(|error| format!("Failed to configure remote handshake write deadline: {error}"))
}

fn read_message<T: for<'de> Deserialize<'de>, R: Read>(stream: &mut R) -> Result<T, String> {
    read_message_until_cancelled(stream, || false)
}

fn read_message_until_cancelled<T: for<'de> Deserialize<'de>, R: Read, C: FnMut() -> bool>(
    stream: &mut R,
    mut is_cancelled: C,
) -> Result<T, String> {
    let mut buffer = Vec::new();
    loop {
        if is_cancelled() {
            return Err("Read cancelled because the remote host stopped.".to_string());
        }
        if let Some(message) = try_read_message(stream, &mut buffer)? {
            return Ok(message);
        }
    }
}

fn read_message_until_deadline<T: for<'de> Deserialize<'de>, R: Read>(
    stream: &mut R,
    deadline: Instant,
) -> Result<T, String> {
    let mut buffer = Vec::new();
    loop {
        if Instant::now() >= deadline {
            return Err("Remote handshake read deadline expired.".to_string());
        }
        if let Some(message) = try_read_message(stream, &mut buffer)? {
            return Ok(message);
        }
    }
}

fn read_message_until_deadline_cancelled<
    T: for<'de> Deserialize<'de>,
    R: Read,
    C: FnMut() -> bool,
>(
    stream: &mut R,
    deadline: Instant,
    mut is_cancelled: C,
) -> Result<T, String> {
    let mut buffer = Vec::new();
    loop {
        if is_cancelled() {
            return Err("Remote handshake read cancelled.".to_string());
        }
        if Instant::now() >= deadline {
            return Err("Remote handshake read deadline expired.".to_string());
        }
        if let Some(message) = try_read_message(stream, &mut buffer)? {
            return Ok(message);
        }
    }
}

fn read_client_message_until_deadline_cancelled<T: for<'de> Deserialize<'de>>(
    stream: &mut transport::ClientTlsStream,
    deadline: Instant,
    cancellation: &ForwardCancellation,
) -> Result<T, String> {
    let mut buffer = Vec::new();
    loop {
        if cancellation.is_cancelled() {
            return Err("Remote handshake read cancelled.".to_string());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Remote handshake read deadline expired.".to_string());
        }
        stream
            .sock
            .set_read_timeout(Some(remaining.min(Duration::from_millis(50))))
            .map_err(|error| format!("Failed to configure handshake read timeout: {error}"))?;
        if let Some(message) = try_read_message(stream, &mut buffer)? {
            return Ok(message);
        }
    }
}

fn try_read_message<T: for<'de> Deserialize<'de>, R: Read>(
    stream: &mut R,
    buffer: &mut Vec<u8>,
) -> Result<Option<T>, String> {
    if let Some(message) = try_decode_message(buffer)? {
        return Ok(Some(message));
    }

    let mut chunk = [0_u8; 8192];
    match stream.read(&mut chunk) {
        Ok(0) => Err("Connection closed.".to_string()),
        Ok(bytes_read) => {
            buffer.extend_from_slice(&chunk[..bytes_read]);
            try_decode_message(buffer)
        }
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(format!("Read failed: {error}")),
    }
}

fn try_decode_message<T: for<'de> Deserialize<'de>>(
    buffer: &mut Vec<u8>,
) -> Result<Option<T>, String> {
    if buffer.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes(
        buffer[0..4]
            .try_into()
            .map_err(|_| "Invalid remote frame header.".to_string())?,
    ) as usize;
    if buffer.len() < 4 + len {
        return Ok(None);
    }
    let payload = buffer[4..4 + len].to_vec();
    buffer.drain(0..4 + len);
    from_messagepack_slice(&payload)
        .map(Some)
        .map_err(|error| format!("Parse failed: {error}"))
}

pub(crate) fn stable_hash<T: Serialize>(value: &T) -> u64 {
    let bytes = to_vec_named(value).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn base_snapshot_without_session_views(
    inner: &Arc<RemoteHostInner>,
    client_id: &str,
) -> RemoteWorkspaceSnapshot {
    let app_state = inner
        .shared_state
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let runtime_state = inner
        .runtime_state
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let port_statuses = inner
        .port_statuses
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let raw_port_authorities = inner
        .port_authorities
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let managed_port_snapshots = inner
        .managed_port_snapshots
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let observation_time = Instant::now();
    let deadline = observation_time
        .checked_add(crate::process::ports::DEFAULT_MEMBERSHIP_MAX_AGE)
        .unwrap_or(observation_time);
    let port_authorities = host_verified_port_authorities_at(
        &raw_port_authorities,
        &runtime_state,
        &managed_port_snapshots,
        now_epoch_ms(),
        observation_time,
        deadline,
    );
    let config = inner
        .config
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let controller_client_id = inner
        .controller_client_id
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();

    RemoteWorkspaceSnapshot {
        app_state,
        runtime_state,
        session_views: HashMap::new(),
        port_statuses,
        port_authorities,
        you_have_control: controller_client_id.as_deref() == Some(client_id),
        controller_client_id,
        server_id: config.server_id,
    }
}

pub(crate) fn light_snapshot(
    inner: &Arc<RemoteHostInner>,
    client_id: &str,
) -> RemoteWorkspaceSnapshot {
    let _snapshot_guard = inner
        .snapshot_state_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    base_snapshot_without_session_views(inner, client_id)
}

/// Capture a light snapshot when the caller already owns the snapshot-state
/// lock. Keeping this seam explicit prevents a reentrant mutex acquisition in
/// the browser replay capture path while preserving one coherent authority
/// read for normal callers.
pub(crate) fn light_snapshot_locked(
    inner: &Arc<RemoteHostInner>,
    client_id: &str,
) -> RemoteWorkspaceSnapshot {
    base_snapshot_without_session_views(inner, client_id)
}

#[cfg(test)]
pub(crate) fn current_snapshot(
    inner: &Arc<RemoteHostInner>,
    client_id: &str,
) -> RemoteWorkspaceSnapshot {
    let _snapshot_guard = inner
        .snapshot_state_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut snapshot = base_snapshot_without_session_views(inner, client_id);
    let subscribed_session_ids = session_ids_for_open_tabs(&snapshot.app_state);
    snapshot.session_views = inner
        .session_bootstrap_provider
        .read()
        .ok()
        .and_then(|provider| provider.as_ref().cloned())
        .map(|provider| {
            subscribed_session_ids
                .iter()
                .filter_map(|session_id| provider(session_id))
                .map(|bootstrap| {
                    (
                        bootstrap.session_id.clone(),
                        TerminalSessionView {
                            runtime: bootstrap.runtime,
                            screen: bootstrap.screen,
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    snapshot
}

fn apply_workspace_delta(snapshot: &mut RemoteWorkspaceSnapshot, delta: RemoteWorkspaceDelta) {
    if let Some(app_state) = delta.app_state {
        snapshot.app_state = app_state;
    }
    if let Some(runtime_state) = delta.runtime_state {
        snapshot.runtime_state = runtime_state;
    }
    if let Some(port_statuses) = delta.port_statuses {
        snapshot.port_statuses = port_statuses;
    }
    if let Some(port_authorities) = delta.port_authorities {
        snapshot.port_authorities = port_authorities;
    }
    snapshot.controller_client_id = delta.controller_client_id;
    snapshot.you_have_control = delta.you_have_control;
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::{now_epoch_ms, remote_state_path};
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    static TEST_PROFILE_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) struct TestProfileEnvGuard {
        previous_profile: Option<String>,
        _lock: MutexGuard<'static, ()>,
    }

    impl TestProfileEnvGuard {
        fn with_profile(profile: Option<String>) -> Self {
            // A panicking profile-sensitive test still drops its guard and
            // restores the environment. Recover the serialization lock so
            // one failed assertion cannot fabricate a cascade of unrelated
            // profile failures in the required serial suite.
            let lock = TEST_PROFILE_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous_profile = std::env::var("DEVMANAGER_PROFILE").ok();
            if let Some(profile) = profile.as_ref() {
                std::env::set_var("DEVMANAGER_PROFILE", profile);
            } else {
                std::env::remove_var("DEVMANAGER_PROFILE");
            }
            Self {
                previous_profile,
                _lock: lock,
            }
        }

        pub(crate) fn new(label: &str) -> Self {
            let profile = format!("{label}-{}-{}", std::process::id(), now_epoch_ms());
            Self::with_profile(Some(profile))
        }

        pub(crate) fn without_profile() -> Self {
            Self::with_profile(None)
        }
    }

    impl Drop for TestProfileEnvGuard {
        fn drop(&mut self) {
            if let Some(previous_profile) = self.previous_profile.as_ref() {
                std::env::set_var("DEVMANAGER_PROFILE", previous_profile);
            } else {
                std::env::remove_var("DEVMANAGER_PROFILE");
            }
        }
    }

    pub(crate) struct TestProfileGuard {
        remote_state_dir: PathBuf,
        _env: TestProfileEnvGuard,
    }

    impl TestProfileGuard {
        pub(crate) fn new(label: &str) -> Self {
            let env = TestProfileEnvGuard::new(label);
            let remote_state_dir = remote_state_path()
                .expect("remote state path")
                .parent()
                .expect("remote state dir")
                .to_path_buf();
            let _ = std::fs::remove_dir_all(&remote_state_dir);
            Self {
                remote_state_dir,
                _env: env,
            }
        }
    }

    impl Drop for TestProfileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.remote_state_dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::TestProfileGuard;
    use super::{
        admit_native_client, apply_remote_session_output, apply_workspace_delta,
        authenticate_client, copy_bidirectional, current_controller_allows, current_snapshot,
        deliver_live_semantic_events, deliver_pending_bootstraps, drain_web_clients_for_restart,
        enqueue_deferred_remote_worker_with_reaper, finish_deferred_remote_worker,
        format_handshake_stage_error, generate_pairing_token, handle_client_connection,
        light_snapshot, load_remote_machine_state, native_connection_should_stop, now_epoch_ms,
        prepare_native_client_authentication, publish_semantic_event, read_message,
        read_message_until_cancelled, remote_state_path, remote_worker_reaper_signal,
        request_timeout_for_action, requires_control, run_bounded_remote_callback, run_broadcaster,
        save_remote_known_hosts, save_remote_machine_state, set_last_connection_note,
        spawn_native_connection_worker, try_enqueue_pending_request, upsert_known_host,
        write_message, ClientAuth, ClientMessage, ClientRegistrationTestEvent,
        ConnectedRemoteClient, DeferredRemoteWorker, DeferredRemoteWorkerAdmission,
        DeferredRemoteWorkerOwner, ForwardCancellation, HostConfigAdmissionError,
        HostConfigPersistenceTestPhase, KnownRemoteHost, LocalPortForwardLifecycleTestEvent,
        LocalPortForwardManager, PairedRemoteClient, PairedWebClient, PendingRemoteRequest,
        RegisteredWebPushSender, RemoteAccessActivityEvent, RemoteAccessActivityKind,
        RemoteAccessSource, RemoteAction, RemoteClientHandle, RemoteClientInner, RemoteHostConfig,
        RemoteHostService, RemoteHostWorkLimiter, RemoteLatencyStats, RemoteListenerIdentity,
        RemoteMachineState, RemotePortAuthority, RemotePortAuthorityKind, RemoteSessionBootstrap,
        RemoteSessionStreamEvent, RemoteStatePersistenceIoTestPhase, RemoteTerminalInput,
        RemoteWorker, RemoteWorkerAdmissionPool, RemoteWorkerReaper, RemoteWorkerSpawnError,
        RemoteWorkspaceDelta, RemoteWorkspaceSnapshot, ServerMessage,
        HOST_CONFIG_PERSISTENCE_TEST_HOOK, MAX_PENDING_REMOTE_REQUESTS, PROTOCOL_VERSION,
        REMOTE_PORT_AUTHORITY_MAX_AGE_MS, REMOTE_STATE_PERMISSION_VERIFY_TEST_HOOK,
        REMOTE_STATE_PERSISTENCE_IO_TEST_HOOK,
    };
    use crate::domain::id::ResourceId;
    use crate::domain::operation::ResourceFence;
    use crate::models::{PortStatus, SessionTab, TabType};
    use crate::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
    use crate::process::ports::{
        test_capability_from_snapshot, ManagedResourceSnapshot, RegistryMembershipSnapshot,
    };
    use crate::process::registry::ManagedProcessState;
    use crate::remote::presentation::{
        JournalLimits, SemanticAdapterHealth, SemanticAttention, SemanticEventDraft,
        SemanticEventKind, SemanticJournalStore, SemanticRetention, SemanticSource,
        StableSessionKey,
    };
    use crate::remote::web::bridge::BrowserOutboundSender;
    use crate::remote::web::push::{
        validate_registration, PushAttentionKind, PushDelivery, PushRegistrationKeys,
        PushRegistrationMode, PushRegistrationRequest, PushSender,
    };
    use crate::remote::web::wire::WsOutbound;
    use crate::state::{
        AppState, RuntimeState, SessionDimensions, SessionKind, SessionRuntimeState, SessionStatus,
    };
    use crate::terminal::session::{
        TerminalBackend, TerminalCellSnapshot, TerminalModeSnapshot, TerminalScreenSnapshot,
        TerminalSessionView,
    };
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::io::{ErrorKind, Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex, RwLock};
    use std::thread;
    use std::time::{Duration, Instant};

    fn read_proves_socket_closed(stream: &mut TcpStream) -> bool {
        let mut byte = [0_u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => true,
            Ok(_) => false,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                false
            }
            Err(_) => true,
        }
    }

    struct HostConfigPersistenceHookGuard;

    impl HostConfigPersistenceHookGuard {
        fn install(
            hook: Arc<
                dyn Fn(&RemoteHostConfig, HostConfigPersistenceTestPhase) -> std::io::Result<()>
                    + Send
                    + Sync,
            >,
        ) -> Self {
            let mut slot = HOST_CONFIG_PERSISTENCE_TEST_HOOK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert!(slot.is_none(), "host config persistence test hook leaked");
            *slot = Some(hook);
            Self
        }
    }

    impl Drop for HostConfigPersistenceHookGuard {
        fn drop(&mut self) {
            *HOST_CONFIG_PERSISTENCE_TEST_HOOK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
    }

    struct RemoteStatePermissionVerifyHookGuard;

    impl RemoteStatePermissionVerifyHookGuard {
        fn install(hook: Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>) -> Self {
            let mut slot = REMOTE_STATE_PERMISSION_VERIFY_TEST_HOOK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert!(slot.is_none(), "remote state permission hook leaked");
            *slot = Some(hook);
            Self
        }
    }

    impl Drop for RemoteStatePermissionVerifyHookGuard {
        fn drop(&mut self) {
            *REMOTE_STATE_PERMISSION_VERIFY_TEST_HOOK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
    }

    struct RemoteStatePersistenceIoHookGuard;

    impl RemoteStatePersistenceIoHookGuard {
        fn install(
            hook: Arc<
                dyn Fn(RemoteStatePersistenceIoTestPhase, &Path) -> std::io::Result<()>
                    + Send
                    + Sync,
            >,
        ) -> Self {
            let mut slot = REMOTE_STATE_PERSISTENCE_IO_TEST_HOOK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert!(slot.is_none(), "remote state IO hook leaked");
            *slot = Some(hook);
            Self
        }
    }

    impl Drop for RemoteStatePersistenceIoHookGuard {
        fn drop(&mut self) {
            *REMOTE_STATE_PERSISTENCE_IO_TEST_HOOK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
    }

    fn test_terminal_screen(text: &str) -> TerminalScreenSnapshot {
        let mut snapshot = TerminalScreenSnapshot::default();
        snapshot.lines = text
            .split('\n')
            .map(|line| {
                line.chars()
                    .map(|character| TerminalCellSnapshot {
                        character,
                        zero_width: Vec::new(),
                        foreground: 0,
                        background: 0,
                        bold: false,
                        dim: false,
                        italic: false,
                        underline: false,
                        undercurl: false,
                        strike: false,
                        hidden: false,
                        has_hyperlink: false,
                        default_background: true,
                        default_foreground: false,
                    })
                    .collect()
            })
            .collect();
        snapshot.rows = snapshot.lines.len();
        snapshot.cols = text.lines().map(str::len).max().unwrap_or_default();
        snapshot
    }

    #[test]
    fn pairing_token_uses_eight_unambiguous_characters() {
        let token = generate_pairing_token();
        assert_eq!(token.len(), 8);
        assert!(
            token
                .bytes()
                .all(|byte| b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789".contains(&byte)),
            "pairing token contained an ambiguous or unsafe character: {token}"
        );
    }

    #[test]
    fn native_secret_uses_full_width_random_hex() {
        let secret = super::generate_secret("auth");
        let random_hex = secret
            .strip_prefix("auth-")
            .expect("secret should retain its namespace");
        assert_eq!(random_hex.len(), 48);
        assert!(random_hex.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn remote_machine_defaults_include_host_config() {
        let state = RemoteMachineState::default();
        assert!(!state.host.server_id.is_empty());
        assert!(!state.host.pairing_token.is_empty());
        assert_eq!(state.host.port, 43871);
    }

    #[test]
    fn native_terminal_input_origins_round_trip_without_losing_provenance() {
        let inputs = [
            RemoteTerminalInput::Text {
                session_id: "session-text".to_string(),
                text: "typed".to_string(),
            },
            RemoteTerminalInput::Paste {
                session_id: "session-paste".to_string(),
                text: "pasted".to_string(),
            },
            RemoteTerminalInput::Bytes {
                session_id: "session-bytes".to_string(),
                bytes: b"\x1b[A".to_vec(),
            },
            RemoteTerminalInput::Control {
                session_id: "session-control".to_string(),
                bytes: b"\x03".to_vec(),
            },
        ];

        for (index, input) in inputs.into_iter().enumerate() {
            let encoded = rmp_serde::encode::to_vec_named(&ClientMessage::TerminalInput {
                input,
                enqueued_at_epoch_ms: 42,
            })
            .expect("encode native terminal input");
            let decoded: ClientMessage =
                rmp_serde::decode::from_slice(&encoded).expect("decode native terminal input");
            let ClientMessage::TerminalInput {
                input,
                enqueued_at_epoch_ms,
            } = decoded
            else {
                panic!("expected terminal input");
            };
            assert_eq!(enqueued_at_epoch_ms, 42);
            match (index, input) {
                (0, RemoteTerminalInput::Text { session_id, text }) => {
                    assert_eq!(session_id, "session-text");
                    assert_eq!(text, "typed");
                }
                (1, RemoteTerminalInput::Paste { session_id, text }) => {
                    assert_eq!(session_id, "session-paste");
                    assert_eq!(text, "pasted");
                }
                (2, RemoteTerminalInput::Bytes { session_id, bytes }) => {
                    assert_eq!(session_id, "session-bytes");
                    assert_eq!(bytes, b"\x1b[A");
                }
                (3, RemoteTerminalInput::Control { session_id, bytes }) => {
                    assert_eq!(session_id, "session-control");
                    assert_eq!(bytes, b"\x03");
                }
                (_, other) => panic!("input origin changed during round trip: {other:?}"),
            }
        }
    }

    #[test]
    fn host_config_defaults_to_disabled_hosting() {
        let config = RemoteHostConfig::default();
        assert!(!config.enabled);
        assert!(!config.certificate_pem.is_empty());
        assert!(!config.private_key_pem.is_empty());
        assert!(!config.certificate_fingerprint.is_empty());
    }

    #[test]
    fn dropping_nonfinal_service_clone_keeps_shared_runtime_alive() {
        let service = RemoteHostService::new(RemoteHostConfig::default());

        drop(service.clone());

        assert!(!service.inner.stop_flag.load(Ordering::SeqCst));
    }

    #[test]
    fn weak_callback_handle_does_not_retain_the_stopped_host_runtime() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let callback_handle = service.downgrade();
        assert!(callback_handle.upgrade().is_some());

        drop(service);

        assert!(
            callback_handle.upgrade().is_none(),
            "a non-owning callback payload retained the stopped host runtime"
        );
    }

    #[test]
    fn bounded_callback_observes_host_cancellation_before_its_deadline() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let generation = service
            .inner
            .native_runtime_generation
            .load(Ordering::Acquire);
        let (callback_entered_tx, callback_entered_rx) = mpsc::sync_channel(1);
        let (callback_release_tx, callback_release_rx) = mpsc::sync_channel(0);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let (reaped_tx, reaped_rx) = mpsc::sync_channel(1);
        *service
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = Some(reaped_tx);
        let callback_inner = service.inner.clone();
        let caller = thread::spawn(move || {
            let result = run_bounded_remote_callback(
                &callback_inner,
                generation,
                "test-cancelled-remote-callback",
                move || {
                    callback_entered_tx
                        .send(())
                        .expect("callback observer should remain");
                    callback_release_rx
                        .recv_timeout(Duration::from_secs(3))
                        .expect("callback should be released");
                    7_u8
                },
            );
            result_tx
                .send(result)
                .expect("callback result observer should remain");
        });
        callback_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("callback should start");

        service.inner.stop_flag.store(true, Ordering::Release);
        assert_eq!(
            result_rx
                .recv_timeout(Duration::from_millis(250))
                .expect("cancellation should interrupt the callback wait"),
            None
        );
        callback_release_tx
            .send(())
            .expect("callback worker should remain owned");
        caller.join().expect("callback caller should join");
        let reaped = reaped_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("cancelled callback worker should be reaped");
        assert_eq!(reaped.name, "test-cancelled-remote-callback");
        *service
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = None;
    }

    #[test]
    fn dropping_root_service_stops_runtime_with_clone_backed_handler_alive() {
        let root = RemoteHostService::new(RemoteHostConfig::default());
        let ordinary_clone = root.clone();
        let handler_clone = root.clone();
        let internal_reference = root.inner.clone();
        root.set_terminal_input_handler(Some(Arc::new(move |_input, _enqueued_at_epoch_ms| {
            let _ = handler_clone.status();
            Ok(())
        })));
        assert!(internal_reference
            .terminal_input_handler
            .read()
            .expect("terminal input handler lock")
            .is_some());

        drop(root);

        assert!(internal_reference.stop_flag.load(Ordering::SeqCst));
        assert!(internal_reference
            .terminal_input_handler
            .read()
            .expect("terminal input handler lock")
            .is_none());
        drop(ordinary_clone);
    }

    #[test]
    fn dropping_root_service_closes_web_listener_with_clone_backed_handler_alive() {
        let port = reserve_free_tcp_port();
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        config.web.bind_address = "127.0.0.1".to_string();
        config.web.port = port;
        let root = RemoteHostService::new(config);
        assert!(
            TcpListener::bind(("127.0.0.1", port)).is_err(),
            "web listener did not reserve its configured port"
        );
        let ordinary_clone = root.clone();
        let handler_clone = root.clone();
        root.set_terminal_input_handler(Some(Arc::new(move |_input, _enqueued_at_epoch_ms| {
            let _ = handler_clone.status();
            Ok(())
        })));

        drop(root);

        wait_for(
            || TcpListener::bind(("127.0.0.1", port)).is_ok(),
            Duration::from_secs(3),
            "root service drop left the browser listener port bound",
        );
        ordinary_clone.set_terminal_input_handler(None);
    }

    #[test]
    fn dropping_root_service_revokes_registered_browser_authority() {
        let root = RemoteHostService::new(RemoteHostConfig::default());
        let internal_reference = root.inner.clone();
        let (native_tx, _native_rx) = mpsc::channel();
        let web_sender = BrowserOutboundSender::detached_for_test(8, 1024 * 1024);
        let tombstone = web_sender.tombstone();
        internal_reference
            .clients
            .lock()
            .expect("clients lock")
            .insert(
                1,
                test_connected_client("browser", native_tx, Some(web_sender)),
            );
        internal_reference
            .web_control
            .lock()
            .expect("web control lock")
            .writer_leases_mut()
            .acquire(1, "browser", "tab", now_epoch_ms())
            .expect("browser lease");
        *internal_reference
            .controller_client_id
            .write()
            .expect("controller lock") = Some("browser".to_string());

        drop(root);

        assert!(
            !tombstone.is_active(),
            "root drop left a browser mutation tombstone authoritative"
        );
        assert!(
            internal_reference
                .clients
                .lock()
                .expect("clients lock")
                .is_empty(),
            "root drop retained a registered browser"
        );
        assert!(
            internal_reference
                .web_control
                .lock()
                .expect("web control lock")
                .writer_leases()
                .peek()
                .is_none(),
            "root drop retained the browser writer lease"
        );
        assert!(
            internal_reference
                .controller_client_id
                .read()
                .expect("controller lock")
                .is_none(),
            "root drop retained the browser controller"
        );
    }

    #[test]
    fn dropping_root_service_releases_a_stalled_native_tls_worker() {
        let port = reserve_free_tcp_port();
        let mut config = RemoteHostConfig {
            enabled: false,
            bind_address: "127.0.0.1".to_string(),
            port,
            ..RemoteHostConfig::default()
        };
        let root = RemoteHostService::new(config.clone());
        let (listener_started_tx, listener_started_rx) = mpsc::sync_channel(1);
        *root
            .inner
            .native_lifecycle_test_hook
            .write()
            .expect("native lifecycle hook lock") = Some(Arc::new(move |event| {
            if event == super::NativeLifecycleTestEvent::ListenerStarted {
                listener_started_tx
                    .send(())
                    .expect("listener observer should remain");
            }
        }));
        let (worker_admitted_tx, worker_admitted_rx) = mpsc::sync_channel(1);
        *root
            .inner
            .native_worker_registration_test_hook
            .write()
            .expect("native worker registration hook lock") = Some(Arc::new(move || {
            worker_admitted_tx
                .send(())
                .expect("worker admission observer should remain");
        }));
        config.enabled = true;
        root.apply_config(config);
        listener_started_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("native listener never started");

        let stalled_client =
            TcpStream::connect(("127.0.0.1", port)).expect("stalled native client should connect");
        worker_admitted_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("native listener never admitted the stalled TLS worker");
        let inner = Arc::downgrade(&root.inner);

        drop(root);

        assert!(
            inner.upgrade().is_none(),
            "stalled native TLS worker retained the stopped host runtime"
        );
        drop(stalled_client);
    }

    #[test]
    fn dropping_root_service_releases_a_tls_client_that_withholds_hello() {
        let port = reserve_free_tcp_port();
        let mut config = RemoteHostConfig {
            enabled: false,
            bind_address: "127.0.0.1".to_string(),
            port,
            ..RemoteHostConfig::default()
        };
        let root = RemoteHostService::new(config.clone());
        let (listener_started_tx, listener_started_rx) = mpsc::sync_channel(1);
        *root
            .inner
            .native_lifecycle_test_hook
            .write()
            .expect("native lifecycle hook lock") = Some(Arc::new(move |event| {
            if event == super::NativeLifecycleTestEvent::ListenerStarted {
                listener_started_tx
                    .send(())
                    .expect("listener observer should remain");
            }
        }));
        config.enabled = true;
        root.apply_config(config);
        listener_started_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("native listener never started");
        let stalled_client = super::transport::connect_tls("127.0.0.1", port, None)
            .expect("TLS-only native client should complete transport handshake")
            .stream;
        let inner = Arc::downgrade(&root.inner);

        drop(root);

        assert!(
            inner.upgrade().is_none(),
            "TLS client that withheld hello retained the stopped host runtime"
        );
        drop(stalled_client);
    }

    #[test]
    fn stale_native_runtime_generation_stays_stopped_after_flag_reset() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let admitted_generation = service
            .inner
            .native_runtime_generation
            .load(Ordering::SeqCst);
        assert!(!native_connection_should_stop(
            &service.inner,
            admitted_generation
        ));

        service
            .inner
            .native_runtime_generation
            .fetch_add(1, Ordering::SeqCst);
        service.inner.stop_flag.store(false, Ordering::SeqCst);

        assert!(native_connection_should_stop(
            &service.inner,
            admitted_generation
        ));
    }

    #[test]
    fn lightweight_remote_actions_use_default_request_timeout() {
        assert_eq!(
            request_timeout_for_action(&RemoteAction::GitListRepos),
            super::REQUEST_TIMEOUT
        );
        assert_eq!(
            request_timeout_for_action(&RemoteAction::StopAllServers),
            super::REQUEST_TIMEOUT
        );
    }

    #[test]
    fn ai_lifecycle_actions_allow_slow_provider_startup() {
        let dimensions = SessionDimensions::default();
        for action in [
            RemoteAction::LaunchAi {
                project_id: "project".to_string(),
                tab_type: TabType::Codex,
                dimensions,
            },
            RemoteAction::OpenAiTab {
                tab_id: "tab".to_string(),
                dimensions,
            },
            RemoteAction::RestartAiTab {
                tab_id: "tab".to_string(),
                dimensions,
            },
        ] {
            assert!(request_timeout_for_action(&action) > super::REQUEST_TIMEOUT);
        }
    }

    #[test]
    fn pending_remote_request_queue_is_bounded() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        for index in 0..MAX_PENDING_REMOTE_REQUESTS {
            assert!(try_enqueue_pending_request(
                &service.inner,
                PendingRemoteRequest {
                    client_id: format!("client-{index}"),
                    git_authority: None,
                    action: RemoteAction::GitListRepos,
                    response: None,
                },
            )
            .is_ok());
        }

        assert!(try_enqueue_pending_request(
            &service.inner,
            PendingRemoteRequest {
                client_id: "overflow".to_string(),
                git_authority: None,
                action: RemoteAction::GitListRepos,
                response: None,
            },
        )
        .is_err());
        assert_eq!(
            service.inner.pending_requests.lock().unwrap().len(),
            MAX_PENDING_REMOTE_REQUESTS
        );
    }

    #[test]
    fn host_work_permits_survive_response_timeouts_until_jobs_finish() {
        let limiter = RemoteHostWorkLimiter::new(2);
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let mut waiters = Vec::new();
        let mut workers = Vec::new();

        for _ in 0..2 {
            let permit = limiter.try_acquire().expect("work slot");
            let active = active.clone();
            let max_active = max_active.clone();
            let entered_tx = entered_tx.clone();
            let release_rx = release_rx.clone();
            let (response_tx, response_rx) = mpsc::channel();
            waiters.push(response_rx);
            workers.push(thread::spawn(move || {
                permit.run(|| {
                    let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now_active, Ordering::SeqCst);
                    entered_tx.send(()).unwrap();
                    release_rx.lock().unwrap().recv().unwrap();
                    active.fetch_sub(1, Ordering::SeqCst);
                    let _ = response_tx.send(());
                });
            }));
        }
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        for waiter in waiters {
            assert_eq!(
                waiter.recv_timeout(Duration::from_millis(10)),
                Err(mpsc::RecvTimeoutError::Timeout)
            );
        }
        assert!(
            limiter.try_acquire().is_none(),
            "response timeout released a permit before Git work completed"
        );
        assert_eq!(max_active.load(Ordering::SeqCst), 2);

        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(limiter.try_acquire().is_some());
    }

    #[test]
    fn network_backed_git_actions_use_extended_request_timeout() {
        let repo_path = "repo".to_string();
        let extended_actions = [
            RemoteAction::GitCommit {
                repo_path: repo_path.clone(),
                summary: "summary".to_string(),
                body: None,
            },
            RemoteAction::GitPush {
                repo_path: repo_path.clone(),
            },
            RemoteAction::GitPushSetUpstream {
                repo_path: repo_path.clone(),
                branch: "main".to_string(),
            },
            RemoteAction::GitSync {
                repo_path: repo_path.clone(),
            },
        ];

        for action in extended_actions {
            assert!(
                request_timeout_for_action(&action) > super::REQUEST_TIMEOUT,
                "{action:?} should use an extended timeout"
            );
        }
    }

    #[test]
    fn git_read_actions_do_not_require_remote_control() {
        let repo_path = "repo".to_string();
        let read_actions = [
            RemoteAction::GitListRepos,
            RemoteAction::GitStatus {
                repo_path: repo_path.clone(),
            },
            RemoteAction::GitLog {
                repo_path: repo_path.clone(),
                limit: 50,
                skip: 0,
            },
            RemoteAction::GitDiffFile {
                repo_path: repo_path.clone(),
                file_path: "src/main.rs".to_string(),
                staged: false,
            },
            RemoteAction::GitDiffCommit {
                repo_path: repo_path.clone(),
                hash: "HEAD".to_string(),
            },
            RemoteAction::GitBranches { repo_path },
        ];

        for action in read_actions {
            assert!(
                !requires_control(&action),
                "{action:?} should be readable without remote control"
            );
        }
    }

    #[test]
    fn git_mutation_actions_require_remote_control() {
        let repo_path = "repo".to_string();
        let mutation_actions = [
            RemoteAction::GitCommit {
                repo_path: repo_path.clone(),
                summary: "summary".to_string(),
                body: None,
            },
            RemoteAction::GitPush {
                repo_path: repo_path.clone(),
            },
            RemoteAction::GitPushSetUpstream {
                repo_path: repo_path.clone(),
                branch: "main".to_string(),
            },
            RemoteAction::GitSync { repo_path },
        ];

        for action in mutation_actions {
            assert!(
                requires_control(&action),
                "{action:?} should require remote control"
            );
        }
    }

    #[test]
    fn web_listener_patch_preserves_other_host_settings_and_pairing() {
        let _profile = TestProfileGuard::new("web-listener-narrow-patch");
        let mut before = RemoteMachineState::default();
        before.host.web.cookie_secret_hex = "ab".repeat(32);
        before.host.web.pairing_token = "retained-test-invite".into();
        save_remote_machine_state(&before).expect("seed remote state");
        let before = load_remote_machine_state().expect("normalized seed");
        super::update_web_listener_config(|web| {
            web.port = 18443;
            web.enabled = false;
        })
        .expect("narrow listener patch");
        let mut after = load_remote_machine_state().expect("read patch");
        assert_eq!(after.host.web.port, 18443);
        after.host.web.port = before.host.web.port;
        after.host.web.enabled = before.host.web.enabled;
        assert_eq!(after, before, "only requested web fields may change");
    }

    #[test]
    fn missing_remote_state_remains_first_run_without_persisting() {
        let _profile = TestProfileGuard::new("remote-state-missing");
        let path = super::remote_state_path().expect("remote state path");
        assert!(!path.exists());

        let state = load_remote_machine_state().expect("missing state is first run");

        assert!(!state.host.enabled);
        assert!(!state.host.web.enabled);
        assert!(
            !path.exists(),
            "first-run load must not persist automatically"
        );
    }

    #[test]
    fn malformed_remote_state_returns_error_without_replacing_bytes() {
        let _profile = TestProfileGuard::new("remote-state-malformed");
        let path = super::remote_state_path().expect("remote state path");
        std::fs::create_dir_all(path.parent().expect("remote state directory"))
            .expect("create remote state directory");
        let malformed = b"{ not valid remote json";
        std::fs::write(&path, malformed).expect("write malformed remote state");

        let error = load_remote_machine_state().expect_err("malformed state must fail");

        assert!(matches!(
            error,
            crate::persistence::PersistenceError::Parse { .. }
        ));
        assert_eq!(
            std::fs::read(&path).expect("read preserved malformed state"),
            malformed
        );
    }

    #[test]
    fn remote_machine_state_round_trips_web_pairing_fields() {
        let _profile = TestProfileGuard::new("remote-web-config");
        let mut state = RemoteMachineState::default();
        state.known_hosts.push(KnownRemoteHost {
            label: "Existing".to_string(),
            address: "192.168.0.50".to_string(),
            port: 43871,
            server_id: "host-existing".to_string(),
            certificate_fingerprint: "fp-existing".to_string(),
            client_id: "client-existing".to_string(),
            auth_token: "token-existing".to_string(),
            last_connected_epoch_ms: Some(1),
        });
        state.host.web.cookie_secret_hex = "feedface".repeat(8);
        state.host.web.paired_clients.push(PairedWebClient {
            client_id: "web-client-1".to_string(),
            browser_install_id: "browser-install-1".to_string(),
            nickname: None,
            label: "Phone".to_string(),
            issued_at_epoch_ms: Some(10),
            last_seen_epoch_ms: Some(20),
            last_seen_ip: Some("127.0.0.1".to_string()),
            user_agent: Some("Safari".to_string()),
            browser_family: Some("Safari".to_string()),
            browser_version: Some("17.4".to_string()),
            os_family: Some("iOS".to_string()),
            device_class: Some("phone".to_string()),
            permitted_origin: None,
        });

        save_remote_machine_state(&state).expect("save remote machine state");
        let reloaded = load_remote_machine_state().expect("reload remote machine state");

        assert_eq!(
            reloaded.host.web.cookie_secret_hex,
            state.host.web.cookie_secret_hex
        );
        assert_eq!(
            reloaded.host.web.paired_clients,
            state.host.web.paired_clients
        );
        assert_eq!(reloaded.known_hosts.len(), 1);
        assert_eq!(reloaded.known_hosts[0].server_id, "host-existing");
    }

    #[test]
    fn persisted_remote_machine_state_is_private_to_current_user() {
        let _profile = TestProfileGuard::new("private-remote-state");
        save_remote_machine_state(&RemoteMachineState::default())
            .expect("save remote machine state");
        let path = super::remote_state_path().expect("remote state path");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(&path)
                .expect("remote state metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "unexpected mode for {path:?}");
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;

            let output = std::process::Command::new("icacls")
                .arg(&path)
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .expect("inspect remote state ACL");
            assert!(
                output.status.success(),
                "icacls failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );

            let acl = String::from_utf8_lossy(&output.stdout).to_lowercase();
            assert!(
                !acl.contains("(i)"),
                "remote state retained inherited ACL entries:\n{acl}"
            );
            for broad_principal in [
                "codexsandboxusers",
                "builtin\\users",
                "builtin\\administrators",
                "nt authority\\system",
                "authenticated users",
                "everyone",
            ] {
                assert!(
                    !acl.contains(broad_principal),
                    "remote state grants {broad_principal}:\n{acl}"
                );
            }
            let username = std::env::var("USERNAME").expect("USERNAME");
            let identity = std::env::var("USERDOMAIN")
                .ok()
                .filter(|domain| !domain.trim().is_empty())
                .map(|domain| format!("{domain}\\{username}"))
                .unwrap_or(username)
                .to_lowercase();
            assert!(
                acl.contains(&format!("{identity}:(f)")),
                "remote state does not grant the current user {identity} full control:\n{acl}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn local_administrator_sddl_alias_matches_only_rid_500() {
        assert!(super::windows_trustee_matches_sid(
            "LA",
            "S-1-5-21-111-222-333-500"
        ));
        assert!(!super::windows_trustee_matches_sid(
            "LA",
            "S-1-5-21-111-222-333-1001"
        ));
    }

    #[cfg(windows)]
    #[test]
    fn new_remote_state_acl_removes_explicit_non_user_grants() {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let _profile = TestProfileGuard::new("explicit-new-remote-acl");
        let path = super::remote_state_path().expect("remote state path");
        std::fs::create_dir_all(path.parent().expect("remote state directory"))
            .expect("create remote state directory");
        std::fs::write(&path, b"{}").expect("seed remote state file");

        let current_sid = super::current_windows_process_sid().expect("current process SID");
        let icacls = super::windows_system_tool("icacls.exe").expect("absolute icacls path");
        let output = std::process::Command::new(icacls)
            .arg(&path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("*{current_sid}:(F)"))
            .arg("/grant")
            .arg("*S-1-5-18:(F)")
            .arg("/grant")
            .arg("*S-1-5-32-544:(F)")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .expect("seed explicit remote state ACL");
        assert!(
            output.status.success(),
            "could not seed explicit remote state ACL: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            super::verify_remote_state_file_permissions(&path).is_err(),
            "test fixture unexpectedly started current-user only"
        );

        super::lock_new_remote_state_file_permissions(&path)
            .expect("new remote state ACL should remove explicit non-user grants");
        super::verify_remote_state_file_permissions(&path)
            .expect("new remote state ACL should be current-user only");
    }

    #[test]
    fn post_rename_permission_failure_restores_disk_bytes_and_memory() {
        let _profile = TestProfileGuard::new("remote-state-post-rename-rollback");
        let mut old_state = RemoteMachineState::default();
        old_state.host.server_id = "old-server".to_string();
        save_remote_machine_state(&old_state).expect("seed remote state");
        let path = super::remote_state_path().expect("remote state path");
        let old_bytes = std::fs::read(&path).expect("read seeded remote state");
        let service = RemoteHostService::new(old_state.host.clone());
        let fail_once = Arc::new(AtomicBool::new(true));
        let _verify_hook = RemoteStatePermissionVerifyHookGuard::install({
            let fail_once = fail_once.clone();
            Arc::new(move |path| {
                if fail_once.swap(false, Ordering::SeqCst) {
                    Err(std::io::Error::new(
                        ErrorKind::PermissionDenied,
                        "injected post-rename permission verification failure",
                    ))
                } else {
                    super::verify_remote_state_file_permissions(path)
                }
            })
        });

        let error = super::mutate_host_config(&service.inner, |config| {
            config.server_id = "new-server".to_string();
        })
        .expect_err("post-rename permission failure must reject the mutation");
        assert!(error.to_string().contains("permission"));
        assert_eq!(
            std::fs::read(&path).expect("read restored remote state"),
            old_bytes
        );
        assert_eq!(service.config().server_id, "old-server");
        assert_eq!(
            load_remote_machine_state()
                .expect("load restored remote state")
                .host
                .server_id,
            "old-server"
        );
    }

    #[test]
    fn durable_remote_save_rolls_back_on_temp_sync_and_rename_failures() {
        let _profile = TestProfileGuard::new("remote-state-durable-io-failures");
        let mut old_state = RemoteMachineState::default();
        old_state.host.server_id = "old-server".to_string();
        save_remote_machine_state(&old_state).expect("seed remote state");
        let path = super::remote_state_path().expect("remote state path");
        let old_bytes = std::fs::read(&path).expect("read seeded remote state");

        for (phase, detail) in [
            (RemoteStatePersistenceIoTestPhase::TempSync, "temp sync"),
            (RemoteStatePersistenceIoTestPhase::Rename, "rename"),
        ] {
            let failed = Arc::new(AtomicBool::new(false));
            let failed_for_hook = failed.clone();
            let _io_hook =
                RemoteStatePersistenceIoHookGuard::install(Arc::new(move |current, _| {
                    if current == phase && !failed_for_hook.swap(true, Ordering::SeqCst) {
                        return Err(std::io::Error::new(
                            ErrorKind::Other,
                            format!("injected {detail} failure"),
                        ));
                    }
                    Ok(())
                }));

            let service = RemoteHostService::new(old_state.host.clone());
            let error = super::mutate_host_config(&service.inner, |config| {
                config.server_id = "new-server".to_string();
            })
            .expect_err("injected durable-save failure must reject mutation");
            assert!(
                error.to_string().contains(detail),
                "unexpected error: {error}"
            );
            assert_eq!(service.config().server_id, "old-server");
            assert_eq!(
                std::fs::read(&path).expect("read unchanged remote state"),
                old_bytes
            );
            assert_eq!(
                load_remote_machine_state()
                    .expect("reopen unchanged remote state")
                    .host
                    .server_id,
                "old-server"
            );
            drop(_io_hook);
        }
    }

    #[test]
    fn durable_remote_save_syncs_parent_and_rolls_back_after_barrier_failure() {
        let _profile = TestProfileGuard::new("remote-state-parent-sync-rollback");
        let mut old_state = RemoteMachineState::default();
        old_state.host.server_id = "old-server".to_string();
        save_remote_machine_state(&old_state).expect("seed remote state");
        let path = super::remote_state_path().expect("remote state path");
        let old_bytes = std::fs::read(&path).expect("read seeded remote state");
        let failed = Arc::new(AtomicBool::new(false));
        let failed_for_hook = failed.clone();
        let _io_hook = RemoteStatePersistenceIoHookGuard::install(Arc::new(move |phase, _| {
            if phase == RemoteStatePersistenceIoTestPhase::ParentSync
                && !failed_for_hook.swap(true, Ordering::SeqCst)
            {
                return Err(std::io::Error::new(
                    ErrorKind::Other,
                    "injected parent sync failure",
                ));
            }
            Ok(())
        }));

        let service = RemoteHostService::new(old_state.host.clone());
        let error = super::mutate_host_config(&service.inner, |config| {
            config.server_id = "new-server".to_string();
        })
        .expect_err("parent barrier failure must reject mutation");
        assert!(
            error.to_string().contains("parent sync"),
            "unexpected error: {error}"
        );
        assert_eq!(service.config().server_id, "old-server");
        assert_eq!(
            std::fs::read(&path).expect("read restored remote state"),
            old_bytes
        );
        assert_eq!(
            load_remote_machine_state()
                .expect("reopen restored remote state")
                .host
                .server_id,
            "old-server"
        );
    }

    #[test]
    fn generic_host_config_commit_failure_compensates_only_its_exact_candidate() {
        let _profile = TestProfileGuard::new("generic-config-commit-compensation");
        let mut base = RemoteHostConfig::default();
        base.server_id = "generic-base".to_string();
        save_remote_machine_state(&RemoteMachineState {
            host: base.clone(),
            known_hosts: Vec::new(),
        })
        .expect("seed generic compensation state");
        let service = RemoteHostService::new(base.clone());
        let injected = Arc::new(AtomicBool::new(false));
        let _persistence_hook = HostConfigPersistenceHookGuard::install(Arc::new({
            let inner = Arc::downgrade(&service.inner);
            let injected = injected.clone();
            move |snapshot, phase| {
                if phase == HostConfigPersistenceTestPhase::AfterWrite
                    && snapshot.server_id == "generic-candidate"
                    && !injected.swap(true, Ordering::SeqCst)
                {
                    inner
                        .upgrade()
                        .expect("generic mutation host should remain")
                        .config_revision
                        .fetch_add(1, Ordering::SeqCst);
                }
                Ok(())
            }
        }));

        let error = super::mutate_host_config(&service.inner, |config| {
            config.server_id = "generic-candidate".to_string();
        })
        .expect_err("stale generic memory commit must reject the mutation");

        assert!(matches!(error, HostConfigAdmissionError::Persistence(_)));
        assert!(injected.load(Ordering::SeqCst));
        assert_eq!(service.config(), base);
        assert_eq!(
            load_remote_machine_state()
                .expect("load compensated generic state")
                .host,
            base,
            "generic commit compensation must restore only its exact durable candidate"
        );
    }

    #[test]
    fn generic_host_config_commit_failure_reports_typed_uncertainty_when_compensation_fails() {
        let _profile = TestProfileGuard::new("generic-config-compensation-uncertain");
        let mut base = RemoteHostConfig::default();
        base.server_id = "uncertain-base".to_string();
        save_remote_machine_state(&RemoteMachineState {
            host: base.clone(),
            known_hosts: Vec::new(),
        })
        .expect("seed generic uncertainty state");
        let service = RemoteHostService::new(base.clone());
        let candidate_written = Arc::new(AtomicBool::new(false));
        let compensation_failed = Arc::new(AtomicBool::new(false));
        let _persistence_hook = HostConfigPersistenceHookGuard::install(Arc::new({
            let inner = Arc::downgrade(&service.inner);
            let candidate_written = candidate_written.clone();
            let compensation_failed = compensation_failed.clone();
            move |snapshot, phase| {
                if phase == HostConfigPersistenceTestPhase::AfterWrite
                    && snapshot.server_id == "uncertain-candidate"
                    && !candidate_written.swap(true, Ordering::SeqCst)
                {
                    inner
                        .upgrade()
                        .expect("generic uncertainty host should remain")
                        .config_revision
                        .fetch_add(1, Ordering::SeqCst);
                }
                if phase == HostConfigPersistenceTestPhase::BeforeWrite
                    && snapshot.server_id == "uncertain-base"
                    && candidate_written.load(Ordering::SeqCst)
                    && !compensation_failed.swap(true, Ordering::SeqCst)
                {
                    return Err(std::io::Error::new(
                        ErrorKind::Other,
                        "injected generic conditional compensation failure",
                    ));
                }
                Ok(())
            }
        }));

        let error = super::mutate_host_config(&service.inner, |config| {
            config.server_id = "uncertain-candidate".to_string();
        })
        .expect_err("failed generic compensation must not report success");

        assert!(matches!(
            error,
            HostConfigAdmissionError::DurabilityUncertain { .. }
        ));
        assert!(candidate_written.load(Ordering::SeqCst));
        assert!(compensation_failed.load(Ordering::SeqCst));
        assert_eq!(service.config(), base);
        assert_eq!(
            load_remote_machine_state()
                .expect("load uncertain generic state")
                .host
                .server_id,
            "uncertain-candidate",
            "typed uncertainty must leave the unresolved exact durable candidate visible"
        );
    }

    #[test]
    fn production_remote_loops_have_no_short_timeout_polling() {
        let source = include_str!("mod.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("remote production source boundary");
        assert!(!production.contains("recv_timeout(Duration::from_millis(25))"));
        assert!(!production.contains("recv_timeout(Duration::from_millis(40))"));
        assert!(!production.contains("wait_timeout(guard, Duration::from_millis(2))"));
        let transport = include_str!("transport.rs");
        assert!(!transport.contains("HANDSHAKE_POLL_INTERVAL"));
        assert!(!transport.contains("ACTIVE_READ_TIMEOUT"));
    }

    #[test]
    fn concurrent_remote_state_saves_do_not_race_on_temp_file() {
        let _profile = TestProfileGuard::new("concurrent-remote-save");

        let threads: Vec<_> = (0..4)
            .map(|_| {
                thread::spawn(|| {
                    for _ in 0..50 {
                        save_remote_machine_state(&RemoteMachineState::default())?;
                    }
                    Ok::<(), crate::persistence::PersistenceError>(())
                })
            })
            .collect();

        for handle in threads {
            handle
                .join()
                .expect("save thread panicked")
                .expect("concurrent saves should all succeed");
        }
    }

    #[test]
    fn native_listener_update_preserves_concurrently_rotated_browser_pairing_state() {
        let _profile = TestProfileGuard::new("listener-preserves-web-pairing");
        let mut config = RemoteHostConfig::default();
        config.web.pairing_token = "browser-token-before".to_string();
        save_remote_machine_state(&RemoteMachineState {
            host: config.clone(),
            known_hosts: Vec::new(),
        })
        .expect("seed remote state");
        let service = RemoteHostService::new(config);
        let listener_port = reserve_free_tcp_port();
        let pairing_service = service.clone();
        let barrier = Arc::new(std::sync::Barrier::new(3));

        let pairing_barrier = barrier.clone();
        let pairing_thread = thread::spawn(move || {
            pairing_barrier.wait();
            super::mutate_host_config(&pairing_service.inner, |config| {
                config.web.pairing_token = "browser-token-after".to_string();
                config.web.paired_clients.push(PairedWebClient {
                    client_id: "web-client-race".to_string(),
                    browser_install_id: "browser-install-race".to_string(),
                    nickname: None,
                    label: "Phone".to_string(),
                    issued_at_epoch_ms: Some(10),
                    last_seen_epoch_ms: Some(20),
                    last_seen_ip: Some("127.0.0.1".to_string()),
                    user_agent: Some("Safari".to_string()),
                    browser_family: Some("Safari".to_string()),
                    browser_version: Some("17.4".to_string()),
                    os_family: Some("iOS".to_string()),
                    device_class: Some("phone".to_string()),
                    permitted_origin: None,
                });
            })
            .expect("persist paired browser");
        });

        let listener_service = service.clone();
        let listener_barrier = barrier.clone();
        let listener_thread = thread::spawn(move || {
            listener_barrier.wait();
            listener_service
                .update_native_listener_settings(true, "127.0.0.1".to_string(), listener_port)
                .expect("update native listener");
        });

        barrier.wait();
        pairing_thread.join().expect("pairing thread");
        listener_thread.join().expect("listener thread");

        let saved = load_remote_machine_state().expect("reload remote state");
        assert!(saved.host.enabled);
        assert_eq!(saved.host.bind_address, "127.0.0.1");
        assert_eq!(saved.host.port, listener_port);
        assert_eq!(saved.host.web.pairing_token, "browser-token-after");
        assert_eq!(saved.host.web.paired_clients.len(), 1);
        assert_eq!(
            saved.host.web.paired_clients[0].client_id,
            "web-client-race"
        );
    }

    #[test]
    fn concurrent_host_and_known_host_saves_preserve_both_fields() {
        let _profile = TestProfileGuard::new("known-host-preserves-host");
        let mut disk_state = RemoteMachineState::default();
        disk_state.host.pairing_token = "rotated-host-token".to_string();
        save_remote_machine_state(&disk_state).expect("seed remote state");

        let mut cached_state = disk_state.clone();
        cached_state.host.pairing_token = "stale-cached-token".to_string();
        cached_state.known_hosts.push(KnownRemoteHost {
            label: "Studio".to_string(),
            address: "10.0.0.5".to_string(),
            port: 43871,
            server_id: "studio-host".to_string(),
            certificate_fingerprint: "fp-studio".to_string(),
            client_id: "client-studio".to_string(),
            auth_token: "auth-studio".to_string(),
            last_connected_epoch_ms: Some(42),
        });

        let service = RemoteHostService::new(disk_state.host.clone());
        let token_service = service.clone();
        let known_hosts = cached_state.known_hosts.clone();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let token_barrier = barrier.clone();
        let token_thread = thread::spawn(move || {
            token_barrier.wait();
            token_service
                .regenerate_native_pairing_token()
                .expect("rotate host token")
        });
        let hosts_barrier = barrier.clone();
        let hosts_thread = thread::spawn(move || {
            hosts_barrier.wait();
            save_remote_known_hosts(&known_hosts).expect("save known hosts only");
        });
        barrier.wait();
        let rotated_token = token_thread.join().expect("token thread");
        hosts_thread.join().expect("known-host thread");

        let saved = load_remote_machine_state().expect("reload remote state");
        assert_eq!(saved.host.pairing_token, rotated_token);
        assert_eq!(saved.known_hosts.len(), 1);
        assert_eq!(saved.known_hosts[0].server_id, "studio-host");
    }

    #[test]
    fn unchanged_browser_listener_settings_do_not_restart_or_revise_service() {
        let _profile = TestProfileGuard::new("unchanged-browser-listener");
        let mut config = RemoteHostConfig::default();
        config.web.bind_address = "127.0.0.1".to_string();
        config.web.port = 43872;
        save_remote_machine_state(&RemoteMachineState {
            host: config.clone(),
            known_hosts: Vec::new(),
        })
        .expect("seed remote state");
        let service = RemoteHostService::new(config);
        let revision = service.config_revision();

        service
            .update_web_listener_settings(false, "127.0.0.1".to_string(), 43872)
            .expect("apply unchanged settings");

        assert_eq!(service.config_revision(), revision);
    }

    #[test]
    fn changed_browser_listener_settings_persist_and_move_the_bound_port() {
        let _profile = TestProfileGuard::new("changed-browser-listener");
        let old_port = reserve_free_tcp_port();
        let mut new_port = reserve_free_tcp_port();
        while new_port == old_port {
            new_port = reserve_free_tcp_port();
        }
        let mut config = RemoteHostConfig::default();
        config.web.enabled = true;
        config.web.bind_address = "127.0.0.1".to_string();
        config.web.port = old_port;
        save_remote_machine_state(&RemoteMachineState {
            host: config.clone(),
            known_hosts: Vec::new(),
        })
        .expect("seed remote state");
        let service = RemoteHostService::new(config);
        assert!(
            TcpListener::bind(("127.0.0.1", old_port)).is_err(),
            "browser listener did not bind its original port"
        );

        service
            .update_web_listener_settings(true, "127.0.0.1".to_string(), new_port)
            .expect("apply changed browser listener settings");

        wait_for(
            || TcpListener::bind(("127.0.0.1", old_port)).is_ok(),
            Duration::from_secs(3),
            "browser listener did not release its original port",
        );
        assert!(
            TcpListener::bind(("127.0.0.1", new_port)).is_err(),
            "browser listener did not bind its new port"
        );
        let saved = load_remote_machine_state().expect("reload remote state");
        assert_eq!(saved.host.web.bind_address, "127.0.0.1");
        assert_eq!(saved.host.web.port, new_port);
    }

    #[cfg(windows)]
    #[test]
    fn loading_legacy_remote_state_upgrades_acl_before_returning_secrets() {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let _profile = TestProfileGuard::new("legacy-remote-acl-upgrade");
        let mut state = RemoteMachineState::default();
        state.host.private_key_pem = "legacy-private-secret".to_string();
        save_remote_machine_state(&state).expect("seed remote state");
        let path = super::remote_state_path().expect("remote state path");
        let icacls = super::windows_system_tool("icacls.exe").expect("absolute icacls path");
        let output = std::process::Command::new(icacls)
            .arg(&path)
            .arg("/grant")
            .arg("*S-1-1-0:(R)")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .expect("weaken legacy ACL");
        assert!(
            output.status.success(),
            "could not create legacy ACL: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let loaded = load_remote_machine_state().expect("legacy ACL should be upgraded");

        assert_eq!(loaded.host.private_key_pem, "legacy-private-secret");
        super::verify_remote_state_file_permissions(&path)
            .expect("upgraded remote state ACL should be current-user only");
    }

    #[test]
    fn native_pairing_persists_issued_credentials_for_restart() {
        let _profile = TestProfileGuard::new("native-pairing-persists");
        let mut config = RemoteHostConfig::default();
        config.pairing_token = "NATIVE-PAIR".to_string();
        save_remote_machine_state(&RemoteMachineState {
            host: config.clone(),
            known_hosts: Vec::new(),
        })
        .expect("seed remote state");
        let service = RemoteHostService::new(config);

        let (client_id, auth_token, _) = authenticate_client(
            &service.inner,
            ClientMessage::Hello {
                protocol_version: super::PROTOCOL_VERSION,
                client_label: "Desktop test".to_string(),
                auth: ClientAuth::PairToken {
                    token: "NATIVE-PAIR".to_string(),
                },
            },
        )
        .expect("native pairing should succeed");

        let reloaded = load_remote_machine_state().expect("reload paired native client");
        assert!(reloaded
            .host
            .paired_clients
            .iter()
            .any(|client| { client.client_id == client_id && client.auth_token == auth_token }));
    }

    #[test]
    fn revoke_paired_client_removes_saved_token_and_control() {
        let _profile = TestProfileGuard::new("revoke-native-client");
        let mut config = RemoteHostConfig::default();
        config.paired_clients.push(PairedRemoteClient {
            client_id: "client-1".to_string(),
            label: "Laptop".to_string(),
            auth_token: "secret".to_string(),
            last_seen_epoch_ms: Some(1),
        });
        save_remote_machine_state(&RemoteMachineState {
            host: config.clone(),
            known_hosts: Vec::new(),
        })
        .expect("seed remote state");
        let service = RemoteHostService::new(config);
        if let Ok(mut controller) = service.inner.controller_client_id.write() {
            *controller = Some("client-1".to_string());
        }

        assert!(service.revoke_paired_client("client-1"));
        assert!(service.config().paired_clients.is_empty());
        assert!(service.status().controller_client_id.is_none());
        assert!(
            load_remote_machine_state()
                .expect("reload remote state")
                .host
                .paired_clients
                .is_empty(),
            "revoked native token was not removed from disk"
        );
    }

    #[test]
    fn revoke_paired_web_client_disconnects_live_browser_and_clears_control() {
        let _profile = TestProfileGuard::new("revoke-web-client");
        let mut config = RemoteHostConfig::default();
        config.web.paired_clients.push(PairedWebClient {
            client_id: "web-client-1".to_string(),
            browser_install_id: "browser-install-1".to_string(),
            nickname: None,
            label: "Browser".to_string(),
            issued_at_epoch_ms: Some(1),
            last_seen_epoch_ms: Some(1),
            last_seen_ip: Some("127.0.0.1".to_string()),
            user_agent: Some("Browser".to_string()),
            browser_family: Some("Chrome".to_string()),
            browser_version: Some("135".to_string()),
            os_family: Some("Windows".to_string()),
            device_class: Some("desktop".to_string()),
            permitted_origin: None,
        });
        let subscription = validate_registration(PushRegistrationRequest {
            mode: PushRegistrationMode::Reconcile,
            endpoint: "https://web.push.apple.com/QM-revoke".to_string(),
            keys: PushRegistrationKeys {
                p256dh: config.web.push.vapid_public_key_base64.clone(),
                auth: URL_SAFE_NO_PAD.encode([5_u8; 16]),
            },
        })
        .expect("valid push subscription");
        config
            .web
            .push
            .enable_and_replace_subscription("web-client-1", subscription, 1)
            .unwrap();
        let service = RemoteHostService::new(config);
        let web_sender = BrowserOutboundSender::detached_for_test(8, 1024 * 1024);
        let tombstone = web_sender.tombstone();

        if let Ok(mut clients) = service.inner.clients.lock() {
            clients.insert(
                1,
                ConnectedRemoteClient {
                    client_id: "web-client-1".to_string(),
                    sender: None,
                    web_sender: Some(web_sender),
                    web_tombstone: Some(tombstone.clone()),
                    semantic_cursors: HashMap::new(),
                    subscribed_session_ids: HashSet::new(),
                    bootstrapped_session_ids: HashSet::new(),
                    bootstrap_pending_session_ids: HashSet::new(),
                    focused_session_id: None,
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                    last_snapshot_revision: 0,
                },
            );
        }
        if let Ok(mut controller) = service.inner.controller_client_id.write() {
            *controller = Some("web-client-1".to_string());
        }

        assert!(service.revoke_paired_web_client("web-client-1"));
        assert!(service.config().web.paired_clients.is_empty());
        assert!(service.config().web.push.subscriptions.is_empty());
        assert!(service.status().controller_client_id.is_none());
        assert!(!tombstone.is_active());
    }

    #[test]
    fn regenerating_browser_invite_preserves_existing_browser_authority() {
        let _profile = TestProfileGuard::new("regenerate-web-invite-preserves-clients");
        let mut config = RemoteHostConfig::default();
        let original_token = config.web.pairing_token.clone();
        config.web.paired_clients.push(PairedWebClient {
            client_id: "web-client-1".to_string(),
            browser_install_id: "browser-install-1".to_string(),
            label: "Phone".to_string(),
            ..PairedWebClient::default()
        });
        let subscription = validate_registration(PushRegistrationRequest {
            mode: PushRegistrationMode::Reconcile,
            endpoint: "https://web.push.apple.com/QM-regenerate".to_string(),
            keys: PushRegistrationKeys {
                p256dh: config.web.push.vapid_public_key_base64.clone(),
                auth: URL_SAFE_NO_PAD.encode([7_u8; 16]),
            },
        })
        .expect("valid push subscription");
        config
            .web
            .push
            .enable_and_replace_subscription("web-client-1", subscription, 1)
            .expect("enable push subscription");
        config.web.activity_log.push(RemoteAccessActivityEvent {
            client_id: "web-client-1".to_string(),
            source: RemoteAccessSource::Browser,
            event_kind: RemoteAccessActivityKind::Connected,
            label: "Phone".to_string(),
            ip_address: Some("127.0.0.1".to_string()),
            event_at_epoch_ms: Some(1),
            browser_family: Some("Safari".to_string()),
            browser_version: Some("18".to_string()),
            os_family: Some("iOS".to_string()),
            device_class: Some("phone".to_string()),
        });
        let original_secret = config.web.cookie_secret_hex.clone();
        let original_push = config.web.push.clone();
        let original_activity = config.web.activity_log.clone();
        let service = RemoteHostService::new(config);

        let new_token = service
            .regenerate_web_pairing_token()
            .expect("regenerate browser invite");
        let saved = service.config();
        let persisted =
            load_remote_machine_state().expect("load persisted regenerated browser invite");

        assert_ne!(new_token, original_token);
        assert_eq!(saved.web.pairing_token, new_token);
        assert_eq!(saved.web.paired_clients.len(), 1);
        assert_eq!(saved.web.cookie_secret_hex, original_secret);
        assert_eq!(saved.web.push, original_push);
        assert_eq!(saved.web.activity_log, original_activity);
        assert_eq!(persisted.host.web, saved.web);
    }

    #[test]
    fn reset_browser_access_rotates_cookie_and_disconnects_live_browsers() {
        let _profile = TestProfileGuard::new("reset-web-access");
        let mut config = RemoteHostConfig::default();
        let original_cookie_secret = config.web.cookie_secret_hex.clone();
        let original_pairing_token = config.web.pairing_token.clone();
        config.web.paired_clients.push(PairedWebClient {
            client_id: "web-client-1".to_string(),
            browser_install_id: "browser-install-1".to_string(),
            nickname: None,
            label: "Browser".to_string(),
            issued_at_epoch_ms: Some(1),
            last_seen_epoch_ms: Some(1),
            last_seen_ip: Some("127.0.0.1".to_string()),
            user_agent: Some("Browser".to_string()),
            browser_family: Some("Chrome".to_string()),
            browser_version: Some("135".to_string()),
            os_family: Some("Windows".to_string()),
            device_class: Some("desktop".to_string()),
            permitted_origin: None,
        });
        let subscription = validate_registration(PushRegistrationRequest {
            mode: PushRegistrationMode::Reconcile,
            endpoint: "https://web.push.apple.com/QM-reset".to_string(),
            keys: PushRegistrationKeys {
                p256dh: config.web.push.vapid_public_key_base64.clone(),
                auth: URL_SAFE_NO_PAD.encode([6_u8; 16]),
            },
        })
        .expect("valid push subscription");
        config
            .web
            .push
            .enable_and_replace_subscription("web-client-1", subscription, 1)
            .unwrap();
        config.web.activity_log.push(RemoteAccessActivityEvent {
            client_id: "web-client-1".to_string(),
            source: RemoteAccessSource::Browser,
            event_kind: RemoteAccessActivityKind::Connected,
            label: "Browser".to_string(),
            ip_address: Some("127.0.0.1".to_string()),
            event_at_epoch_ms: Some(1),
            browser_family: Some("Chrome".to_string()),
            browser_version: Some("135".to_string()),
            os_family: Some("Windows".to_string()),
            device_class: Some("desktop".to_string()),
        });
        config.web.activity_log.push(RemoteAccessActivityEvent {
            client_id: "client-native-1".to_string(),
            source: RemoteAccessSource::NativeApp,
            event_kind: RemoteAccessActivityKind::Connected,
            label: "Studio MacBook".to_string(),
            ip_address: Some("127.0.0.2".to_string()),
            event_at_epoch_ms: Some(2),
            browser_family: None,
            browser_version: None,
            os_family: Some("macOS".to_string()),
            device_class: Some("desktop".to_string()),
        });
        let service = RemoteHostService::new(config);
        let (native_tx, _native_rx) = std::sync::mpsc::channel();
        let web_sender = BrowserOutboundSender::detached_for_test(8, 1024 * 1024);
        let web_tombstone = web_sender.tombstone();

        if let Ok(mut clients) = service.inner.clients.lock() {
            clients.insert(
                1,
                ConnectedRemoteClient {
                    client_id: "web-client-1".to_string(),
                    sender: None,
                    web_sender: Some(web_sender),
                    web_tombstone: Some(web_tombstone.clone()),
                    semantic_cursors: HashMap::new(),
                    subscribed_session_ids: HashSet::new(),
                    bootstrapped_session_ids: HashSet::new(),
                    bootstrap_pending_session_ids: HashSet::new(),
                    focused_session_id: None,
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                    last_snapshot_revision: 0,
                },
            );
            clients.insert(
                2,
                ConnectedRemoteClient {
                    client_id: "client-native-1".to_string(),
                    sender: Some(Arc::new(native_tx)),
                    web_sender: None,
                    web_tombstone: None,
                    semantic_cursors: HashMap::new(),
                    subscribed_session_ids: HashSet::new(),
                    bootstrapped_session_ids: HashSet::new(),
                    bootstrap_pending_session_ids: HashSet::new(),
                    focused_session_id: None,
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                    last_snapshot_revision: 0,
                },
            );
        }
        if let Ok(mut controller) = service.inner.controller_client_id.write() {
            *controller = Some("web-client-1".to_string());
        }

        assert!(service.reset_browser_access());
        let saved = service.config();
        assert!(saved.web.paired_clients.is_empty());
        assert!(saved.web.push.subscriptions.is_empty());
        assert!(!saved.web.push.notifications_enabled("web-client-1"));
        assert_eq!(saved.web.activity_log.len(), 1);
        assert_eq!(
            saved.web.activity_log[0].source,
            RemoteAccessSource::NativeApp
        );
        assert_ne!(saved.web.cookie_secret_hex, original_cookie_secret);
        assert_ne!(saved.web.pairing_token, original_pairing_token);
        assert!(service.status().controller_client_id.is_none());

        assert!(!web_tombstone.is_active());
        assert_eq!(service.status().connected_web_clients, 0);
        assert_eq!(service.status().connected_native_clients, 1);
        let persisted = load_remote_machine_state().expect("load persisted browser reset");
        assert_eq!(persisted.host.web, saved.web);
    }

    #[test]
    fn host_status_splits_live_native_and_web_clients() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (native_tx, _native_rx) = mpsc::channel();
        let web_sender = BrowserOutboundSender::detached_for_test(8, 1024 * 1024);
        let web_tombstone = web_sender.tombstone();

        if let Ok(mut clients) = service.inner.clients.lock() {
            clients.insert(
                1,
                ConnectedRemoteClient {
                    client_id: "client-1".to_string(),
                    sender: Some(Arc::new(native_tx)),
                    web_sender: None,
                    web_tombstone: None,
                    semantic_cursors: HashMap::new(),
                    subscribed_session_ids: HashSet::new(),
                    bootstrapped_session_ids: HashSet::new(),
                    bootstrap_pending_session_ids: HashSet::new(),
                    focused_session_id: None,
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                    last_snapshot_revision: 0,
                },
            );
            clients.insert(
                2,
                ConnectedRemoteClient {
                    client_id: "web-client-1".to_string(),
                    sender: None,
                    web_sender: Some(web_sender),
                    web_tombstone: Some(web_tombstone),
                    semantic_cursors: HashMap::new(),
                    subscribed_session_ids: HashSet::new(),
                    bootstrapped_session_ids: HashSet::new(),
                    bootstrap_pending_session_ids: HashSet::new(),
                    focused_session_id: None,
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                    last_snapshot_revision: 0,
                },
            );
        }

        let status = service.status();

        assert_eq!(status.connected_clients, 2);
        assert_eq!(status.connected_native_clients, 1);
        assert_eq!(status.connected_web_clients, 1);
    }

    #[test]
    fn upsert_known_host_persists_certificate_fingerprint() {
        let mut state = RemoteMachineState::default();
        upsert_known_host(
            &mut state,
            "Studio".to_string(),
            "192.168.0.20".to_string(),
            43871,
            "host-1".to_string(),
            "fingerprint-1".to_string(),
            "client-1".to_string(),
            "token-1".to_string(),
        );

        assert_eq!(state.known_hosts.len(), 1);
        assert_eq!(
            state.known_hosts[0].certificate_fingerprint,
            "fingerprint-1".to_string()
        );
    }

    #[test]
    fn workspace_delta_updates_session_views() {
        let mut snapshot = RemoteWorkspaceSnapshot {
            app_state: AppState::default(),
            runtime_state: RuntimeState::default(),
            session_views: HashMap::from([
                ("old".to_string(), session_view("old")),
                ("keep".to_string(), session_view("keep")),
            ]),
            port_statuses: HashMap::new(),
            port_authorities: HashMap::new(),
            controller_client_id: None,
            you_have_control: false,
            server_id: "host-1".to_string(),
        };

        apply_workspace_delta(
            &mut snapshot,
            RemoteWorkspaceDelta {
                runtime_state: Some(RuntimeState {
                    sessions: HashMap::from([(
                        "runtime-only".to_string(),
                        SessionRuntimeState::new(
                            "runtime-only".to_string(),
                            PathBuf::from("."),
                            SessionDimensions::default(),
                            TerminalBackend::PortablePtyFeedingAlacritty,
                        ),
                    )]),
                    ..RuntimeState::default()
                }),
                controller_client_id: Some("client-1".to_string()),
                you_have_control: true,
                ..Default::default()
            },
        );

        assert!(snapshot.session_views.contains_key("old"));
        assert!(snapshot.session_views.contains_key("keep"));
        assert_eq!(snapshot.runtime_state.sessions.len(), 1);
        assert!(snapshot.runtime_state.sessions.contains_key("runtime-only"));
        assert_eq!(snapshot.controller_client_id.as_deref(), Some("client-1"));
        assert!(snapshot.you_have_control);
    }

    #[test]
    fn push_session_output_only_notifies_subscribed_clients() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (subscribed_tx, subscribed_rx) = mpsc::channel();
        let (idle_tx, idle_rx) = mpsc::channel();

        if let Ok(mut clients) = service.inner.clients.lock() {
            clients.insert(
                1,
                ConnectedRemoteClient {
                    client_id: "client-1".to_string(),
                    sender: Some(Arc::new(subscribed_tx)),
                    web_sender: None,
                    web_tombstone: None,
                    semantic_cursors: HashMap::new(),
                    subscribed_session_ids: HashSet::from(["alpha".to_string()]),
                    bootstrapped_session_ids: HashSet::new(),
                    bootstrap_pending_session_ids: HashSet::new(),
                    focused_session_id: Some("alpha".to_string()),
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                    last_snapshot_revision: 0,
                },
            );
            clients.insert(
                2,
                ConnectedRemoteClient {
                    client_id: "client-2".to_string(),
                    sender: Some(Arc::new(idle_tx)),
                    web_sender: None,
                    web_tombstone: None,
                    semantic_cursors: HashMap::new(),
                    subscribed_session_ids: HashSet::from(["beta".to_string()]),
                    bootstrapped_session_ids: HashSet::new(),
                    bootstrap_pending_session_ids: HashSet::new(),
                    focused_session_id: Some("beta".to_string()),
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                    last_snapshot_revision: 0,
                },
            );
        }

        service.push_session_output("alpha", b"hello".to_vec());

        match subscribed_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ServerMessage::SessionStream {
                event:
                    RemoteSessionStreamEvent::Output {
                        session_id, bytes, ..
                    },
            }) => {
                assert_eq!(session_id, "alpha");
                assert_eq!(bytes, b"hello".to_vec());
            }
            other => panic!("expected output stream event, got {other:?}"),
        }

        assert!(matches!(
            idle_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
    }

    #[test]
    fn ai_raw_output_is_not_recorded_without_raw_terminal_subscribers() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let mut app = AppState::default();
        app.open_tabs.push(SessionTab {
            id: "tab-stable".to_string(),
            tab_type: TabType::Claude,
            project_id: "project-1".to_string(),
            pty_session_id: Some("pty-ephemeral".to_string()),
            provider_session_id: None,
            ..SessionTab::default()
        });
        let mut runtime = SessionRuntimeState::new(
            "pty-ephemeral",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        runtime.session_kind = SessionKind::Claude;
        runtime.tab_id = Some("tab-stable".to_string());
        let mut runtime_state = RuntimeState::default();
        runtime_state
            .sessions
            .insert(runtime.session_id.clone(), runtime);
        service.update_snapshot(app, runtime_state, HashMap::new());

        let before_revision = service.inner.snapshot_revision.load(Ordering::Relaxed);
        service.push_session_output_with_mode(
            "pty-ephemeral",
            b"ok\x1b[3".to_vec(),
            TerminalModeSnapshot::default(),
            Some(test_terminal_screen("ok")),
        );
        service.push_session_output_with_mode(
            "pty-ephemeral",
            b"1mred\x1b[0m\rnext\n".to_vec(),
            TerminalModeSnapshot::default(),
            Some(test_terminal_screen("red\nnext")),
        );

        let replay = service
            .semantic_replay(&StableSessionKey::from_tab("tab-stable"), 0)
            .expect("semantic journal");
        let output = replay
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                SemanticEventKind::Output { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(output.is_empty());
        assert!(service.inner.clients.lock().unwrap().is_empty());
        assert!(service.inner.snapshot_revision.load(Ordering::Relaxed) > before_revision);
    }

    #[test]
    fn native_terminal_modes_are_recorded_without_raw_terminal_subscribers() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let mut app = AppState::default();
        app.open_tabs.push(SessionTab {
            id: "ai-tab".to_string(),
            tab_type: TabType::Claude,
            pty_session_id: Some("ai-runtime".to_string()),
            provider_session_id: None,
            ..SessionTab::default()
        });
        let mut ai_runtime = SessionRuntimeState::new(
            "ai-runtime",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        ai_runtime.session_kind = SessionKind::Claude;
        ai_runtime.tab_id = Some("ai-tab".to_string());
        let mut shell_runtime = SessionRuntimeState::new(
            "shell-runtime",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        shell_runtime.session_kind = SessionKind::Shell;
        shell_runtime.command_id = Some("shell-command".to_string());
        let mut runtime_state = RuntimeState::default();
        runtime_state
            .sessions
            .insert(ai_runtime.session_id.clone(), ai_runtime);
        runtime_state
            .sessions
            .insert(shell_runtime.session_id.clone(), shell_runtime);
        service.update_snapshot(app, runtime_state, HashMap::new());
        let alternate_screen = TerminalModeSnapshot {
            alternate_screen: true,
            ..TerminalModeSnapshot::default()
        };

        service.push_session_output_with_mode("ai-runtime", b"ai".to_vec(), alternate_screen, None);
        service.push_session_output_with_mode(
            "shell-runtime",
            b"shell".to_vec(),
            alternate_screen,
            None,
        );

        assert!(
            !service
                .semantic_session_metadata(&StableSessionKey::from_tab("ai-tab"))
                .expect("AI metadata")
                .raw_required
        );
        assert!(
            service
                .semantic_session_metadata(&StableSessionKey::from_server("shell-command"))
                .expect("shell metadata")
                .raw_required
        );
        assert!(service.inner.clients.lock().unwrap().is_empty());
    }

    #[test]
    fn ai_push_session_output_keeps_screen_snapshots_out_of_conversation() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let mut app = AppState::default();
        app.open_tabs.push(SessionTab {
            id: "ai-tab".to_string(),
            tab_type: TabType::Claude,
            pty_session_id: Some("ai-runtime".to_string()),
            provider_session_id: None,
            ..SessionTab::default()
        });
        let mut ai_runtime = SessionRuntimeState::new(
            "ai-runtime",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        ai_runtime.session_kind = SessionKind::Claude;
        ai_runtime.tab_id = Some("ai-tab".to_string());
        let mut runtime_state = RuntimeState::default();
        runtime_state
            .sessions
            .insert(ai_runtime.session_id.clone(), ai_runtime);
        service.update_snapshot(app, runtime_state, HashMap::new());

        let screen = |text: &str| {
            let mut snapshot = TerminalScreenSnapshot::default();
            snapshot.lines = vec![text
                .chars()
                .map(|character| crate::terminal::session::TerminalCellSnapshot {
                    character,
                    zero_width: Vec::new(),
                    foreground: 0,
                    background: 0,
                    bold: false,
                    dim: false,
                    italic: false,
                    underline: false,
                    undercurl: false,
                    strike: false,
                    hidden: false,
                    has_hyperlink: false,
                    default_background: true,
                    default_foreground: false,
                })
                .collect()];
            snapshot.rows = 1;
            snapshot.cols = text.chars().count();
            snapshot
        };

        service.push_session_output_with_mode(
            "ai-runtime",
            b"frame-1".to_vec(),
            TerminalModeSnapshot::default(),
            Some(screen("frame one")),
        );
        service.push_session_output_with_mode(
            "ai-runtime",
            b"frame-2".to_vec(),
            TerminalModeSnapshot::default(),
            Some(screen("frame two")),
        );
        // Missing screen must not fall back to appending raw AI bytes.
        service.push_session_output("ai-runtime", b"raw-dump-should-not-append".to_vec());

        let replay = service
            .semantic_replay(&StableSessionKey::from_tab("ai-tab"), 0)
            .expect("AI replay");
        let outputs = replay
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                SemanticEventKind::Output { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(outputs.is_empty());
        assert!(replay
            .events
            .iter()
            .all(|event| !matches!(event.kind, SemanticEventKind::Output { .. })));
    }

    #[test]
    fn semantic_projection_runs_outside_the_snapshot_state_lock() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let app = AppState::default();
        let mut runtime = SessionRuntimeState::new(
            "pty-ephemeral",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        runtime.session_kind = SessionKind::Shell;
        runtime.command_id = Some("shell-command".to_string());
        let mut runtime_state = RuntimeState::default();
        runtime_state
            .sessions
            .insert(runtime.session_id.clone(), runtime);
        service.update_snapshot(app, runtime_state, HashMap::new());

        let screen = {
            let mut snapshot = TerminalScreenSnapshot::default();
            snapshot.lines = vec!["projected"
                .chars()
                .map(|character| crate::terminal::session::TerminalCellSnapshot {
                    character,
                    zero_width: Vec::new(),
                    foreground: 0,
                    background: 0,
                    bold: false,
                    dim: false,
                    italic: false,
                    underline: false,
                    undercurl: false,
                    strike: false,
                    hidden: false,
                    has_hyperlink: false,
                    default_background: true,
                    default_foreground: false,
                })
                .collect()];
            snapshot.rows = 1;
            snapshot.cols = 9;
            snapshot
        };
        let snapshot_guard = service.inner.snapshot_state_lock.lock().unwrap();
        let background = service.clone();
        let worker = thread::spawn(move || {
            background.push_session_output_with_mode(
                "pty-ephemeral",
                b"projected".to_vec(),
                TerminalModeSnapshot::default(),
                Some(screen),
            );
        });

        wait_for(
            || {
                service
                    .semantic_replay(&StableSessionKey::from_server("shell-command"), 0)
                    .is_some_and(|replay| {
                        replay.events.iter().any(|event| {
                            matches!(
                                &event.kind,
                                SemanticEventKind::Output { text, .. } if text == "projected"
                            )
                        })
                    })
            },
            Duration::from_millis(250),
            "semantic projection remained blocked behind snapshot state",
        );

        drop(snapshot_guard);
        worker.join().expect("output worker should complete");
    }

    #[test]
    fn runtime_feed_updates_status_attention_and_adapter_metadata_without_clients() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let mut app = AppState::default();
        app.open_tabs.push(SessionTab {
            id: "tab-stable".to_string(),
            tab_type: TabType::Codex,
            project_id: "project-1".to_string(),
            pty_session_id: Some("pty-ephemeral".to_string()),
            provider_session_id: None,
            ..SessionTab::default()
        });
        service.update_snapshot(app, RuntimeState::default(), HashMap::new());
        let mut runtime = SessionRuntimeState::new(
            "pty-ephemeral",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        runtime.session_kind = SessionKind::Codex;
        runtime.tab_id = Some("tab-stable".to_string());
        runtime.status = SessionStatus::Running;
        runtime.unseen_ready = true;
        runtime.notification_count = 3;

        service.push_session_runtime("pty-ephemeral", runtime);

        let key = StableSessionKey::from_tab("tab-stable");
        let metadata = service
            .semantic_session_metadata(&key)
            .expect("semantic metadata");
        assert_eq!(metadata.attention, SemanticAttention::Unread);
        assert_eq!(metadata.attention_count, 3);
        assert_eq!(metadata.adapter_health, SemanticAdapterHealth::Degraded);
        let replay = service.semantic_replay(&key, 0).expect("semantic journal");
        assert!(replay.events.iter().any(|event| matches!(
            &event.kind,
            SemanticEventKind::Status { state, .. } if state == "running"
        )));
        assert!(service.inner.clients.lock().unwrap().is_empty());
    }

    fn service_with_push_subscription(
        client_id: &str,
    ) -> (RemoteHostService, mpsc::Receiver<PushDelivery>) {
        let mut config = RemoteHostConfig::default();
        let subscription = validate_registration(PushRegistrationRequest {
            mode: PushRegistrationMode::Reconcile,
            endpoint: format!("https://web.push.apple.com/QM-{client_id}"),
            keys: PushRegistrationKeys {
                p256dh: config.web.push.vapid_public_key_base64.clone(),
                auth: URL_SAFE_NO_PAD.encode([8_u8; 16]),
            },
        })
        .expect("valid push subscription");
        config
            .web
            .push
            .enable_and_replace_subscription(client_id, subscription, 1)
            .unwrap();
        let service = RemoteHostService::new(config);
        let (sender, receiver) = mpsc::sync_channel(8);
        *service.inner.web_push_sender.write().unwrap() = Some(RegisteredWebPushSender {
            listener_generation: 0,
            sender: PushSender::single(sender),
        });
        (service, receiver)
    }

    fn attention_runtime(
        session_id: &str,
        kind: SessionKind,
        status: SessionStatus,
    ) -> SessionRuntimeState {
        let mut runtime = SessionRuntimeState::new(
            session_id,
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        runtime.session_kind = kind;
        runtime.status = status;
        if matches!(kind, SessionKind::Server | SessionKind::Shell) {
            runtime.command_id = Some(session_id.to_string());
        } else {
            runtime.tab_id = Some(session_id.to_string());
        }
        runtime
    }

    #[test]
    fn unexpected_ssh_disconnect_is_persistently_actionable_before_push_aggregation() {
        let (service, receiver) = service_with_push_subscription("phone-ssh-disconnect");
        let running = attention_runtime("ssh-disconnect", SessionKind::Ssh, SessionStatus::Running);
        service.push_session_runtime("ssh-disconnect", running.clone());

        let mut disconnected = running;
        disconnected.status = SessionStatus::Exited;
        disconnected.exit = Some(crate::state::SessionExitState {
            closed_by_user: false,
            summary: "connection lost".to_string(),
            ..Default::default()
        });
        service.push_session_runtime("ssh-disconnect", disconnected.clone());

        let delivery = receiver
            .recv_timeout(Duration::from_millis(250))
            .expect("unexpected SSH disconnect push");
        assert_eq!(delivery.payload.action, PushAttentionKind::SshDisconnected);
        assert_eq!(delivery.payload.badge, 1);
        let key = StableSessionKey::from_tab("ssh-disconnect");
        assert_eq!(
            service
                .semantic_session_metadata(&key)
                .expect("SSH disconnect metadata")
                .attention,
            SemanticAttention::Failed
        );

        service.push_session_runtime("ssh-disconnect", disconnected);
        assert_eq!(
            service
                .semantic_session_metadata(&key)
                .expect("persistent SSH disconnect metadata")
                .attention,
            SemanticAttention::Failed
        );
        assert!(
            receiver.try_recv().is_err(),
            "disconnect push is deduplicated"
        );
    }

    #[test]
    fn actionable_runtime_transitions_enqueue_once_with_generic_content() {
        let (service, receiver) = service_with_push_subscription("phone-actions");

        let running = attention_runtime("server-a", SessionKind::Server, SessionStatus::Running);
        service.push_session_runtime("server-a", running.clone());
        assert!(receiver.try_recv().is_err());

        let mut crashed = running;
        crashed.status = SessionStatus::Crashed;
        service.push_session_runtime("server-a", crashed.clone());
        let delivery = receiver
            .recv_timeout(Duration::from_millis(250))
            .expect("server crash push");
        assert_eq!(delivery.payload.action, PushAttentionKind::ServerCrashed);
        assert_eq!(delivery.payload.route, "/tasks/server%3Aserver-a");
        assert!(!delivery.payload.body.contains("log"));

        service.push_session_runtime("server-a", crashed);
        assert!(
            receiver.try_recv().is_err(),
            "same transition must not notify twice"
        );

        let mut ai = attention_runtime("claude-a", SessionKind::Claude, SessionStatus::Running);
        service.push_session_runtime("claude-a", ai.clone());
        ai.unseen_ready = true;
        ai.notification_count = 1;
        service.push_session_runtime("claude-a", ai);
        let delivery = receiver
            .recv_timeout(Duration::from_millis(250))
            .expect("AI completion push");
        assert_eq!(delivery.payload.action, PushAttentionKind::Completed);

        let ssh = attention_runtime("ssh-a", SessionKind::Ssh, SessionStatus::Running);
        service.push_session_runtime("ssh-a", ssh.clone());
        let mut disconnected = ssh;
        disconnected.status = SessionStatus::Exited;
        disconnected.exit = Some(crate::state::SessionExitState {
            closed_by_user: false,
            summary: "connection lost".to_string(),
            ..Default::default()
        });
        service.push_session_runtime("ssh-a", disconnected);
        let delivery = receiver
            .recv_timeout(Duration::from_millis(250))
            .expect("unexpected SSH disconnect push");
        assert_eq!(delivery.payload.action, PushAttentionKind::SshDisconnected);

        let user_closed = attention_runtime("ssh-user", SessionKind::Ssh, SessionStatus::Running);
        service.push_session_runtime("ssh-user", user_closed.clone());
        let mut user_closed = user_closed;
        user_closed.status = SessionStatus::Exited;
        user_closed.exit = Some(crate::state::SessionExitState {
            closed_by_user: true,
            summary: "closed".to_string(),
            ..Default::default()
        });
        service.push_session_runtime("ssh-user", user_closed);
        assert!(
            receiver.try_recv().is_err(),
            "an intentional SSH close is not actionable"
        );
        assert_eq!(
            service
                .semantic_session_metadata(&StableSessionKey::from_tab("ssh-user"))
                .expect("intentional SSH close metadata")
                .attention,
            SemanticAttention::None
        );
    }

    #[test]
    fn visibly_focused_install_suppresses_only_its_own_push_subscription() {
        let (service, receiver) = service_with_push_subscription("phone-visible");
        if let Ok(mut clients) = service.inner.clients.lock() {
            clients.insert(
                1,
                ConnectedRemoteClient {
                    client_id: "phone-visible".to_string(),
                    sender: None,
                    web_sender: None,
                    web_tombstone: None,
                    semantic_cursors: HashMap::new(),
                    subscribed_session_ids: HashSet::new(),
                    bootstrapped_session_ids: HashSet::new(),
                    bootstrap_pending_session_ids: HashSet::new(),
                    focused_session_id: Some("server-visible".to_string()),
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                    last_snapshot_revision: 0,
                },
            );
        }
        let running = attention_runtime(
            "server-visible",
            SessionKind::Server,
            SessionStatus::Running,
        );
        service.push_session_runtime("server-visible", running.clone());
        let mut crashed = running;
        crashed.status = SessionStatus::Failed;
        service.push_session_runtime("server-visible", crashed);

        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn visible_install_preserves_host_completion_and_notifies_other_installs() {
        let mut config = RemoteHostConfig::default();
        for (client_id, endpoint) in [
            (
                "phone-visible",
                "https://web.push.apple.com/QM-phone-visible",
            ),
            (
                "tablet-hidden",
                "https://web.push.apple.com/QM-tablet-hidden",
            ),
        ] {
            let subscription = validate_registration(PushRegistrationRequest {
                mode: PushRegistrationMode::Reconcile,
                endpoint: endpoint.to_string(),
                keys: PushRegistrationKeys {
                    p256dh: config.web.push.vapid_public_key_base64.clone(),
                    auth: URL_SAFE_NO_PAD.encode([8_u8; 16]),
                },
            })
            .expect("valid push subscription");
            config
                .web
                .push
                .enable_and_replace_subscription(client_id, subscription, 1)
                .unwrap();
        }
        let service = RemoteHostService::new(config);
        let (sender, receiver) = mpsc::sync_channel(8);
        *service.inner.web_push_sender.write().unwrap() = Some(RegisteredWebPushSender {
            listener_generation: 0,
            sender: PushSender::single(sender),
        });

        let runtime =
            attention_runtime("claude-shared", SessionKind::Claude, SessionStatus::Running);
        service.push_session_runtime("claude-shared", runtime);
        if let Ok(mut clients) = service.inner.clients.lock() {
            clients.insert(
                1,
                ConnectedRemoteClient {
                    client_id: "phone-visible".to_string(),
                    sender: None,
                    web_sender: None,
                    web_tombstone: None,
                    semantic_cursors: HashMap::new(),
                    subscribed_session_ids: HashSet::new(),
                    bootstrapped_session_ids: HashSet::new(),
                    bootstrap_pending_session_ids: HashSet::new(),
                    focused_session_id: Some("claude-shared".to_string()),
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                    last_snapshot_revision: 0,
                },
            );
        }

        let key = StableSessionKey::from_tab("claude-shared");
        service.push_semantic_draft(SemanticEventDraft {
            stable_session_key: key.clone(),
            occurred_at_epoch_ms: 10,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::Status {
                state: "completed".to_string(),
                detail: None,
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: Some("shared-completion".to_string()),
        });

        let delivery = receiver
            .recv_timeout(Duration::from_millis(250))
            .expect("background tablet receives completion push");
        assert_eq!(delivery.subscription.client_id, "tablet-hidden");
        assert_eq!(delivery.payload.action, PushAttentionKind::Completed);
        assert_eq!(
            service
                .semantic_session_metadata(&key)
                .expect("host completion metadata")
                .attention,
            SemanticAttention::Unread
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn semantic_completion_and_question_transitions_notify_without_duplicates() {
        let (service, receiver) = service_with_push_subscription("phone-semantic");
        let runtime = attention_runtime(
            "claude-semantic",
            SessionKind::Claude,
            SessionStatus::Running,
        );
        service.push_session_runtime("claude-semantic", runtime);
        let key = StableSessionKey::from_tab("claude-semantic");

        let completed = SemanticEventDraft {
            stable_session_key: key.clone(),
            occurred_at_epoch_ms: 10,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::Status {
                state: "completed".to_string(),
                detail: None,
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: Some("turn-completed".to_string()),
        };
        service.push_semantic_draft(completed.clone());
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_millis(250))
                .unwrap()
                .payload
                .action,
            PushAttentionKind::Completed
        );
        service.push_semantic_draft(completed);
        assert!(receiver.try_recv().is_err());

        service.publish_semantic_change(|journals| {
            journals.set_attention(&key, SemanticAttention::None, 0)
        });
        let question = SemanticEventDraft {
            stable_session_key: key.clone(),
            occurred_at_epoch_ms: 11,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::Question {
                question_id: "permission-1".to_string(),
                prompt: "PROMPT_SENTINEL".to_string(),
                choices: vec!["Allow".to_string(), "Deny".to_string()],
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: Some("permission-1".to_string()),
        };
        service.push_semantic_draft(question.clone());
        let delivery = receiver
            .recv_timeout(Duration::from_millis(250))
            .expect("question push");
        assert_eq!(delivery.payload.action, PushAttentionKind::NeedsInput);
        assert!(!delivery.payload.body.contains("PROMPT_SENTINEL"));
        service.push_semantic_draft(question);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn removed_session_attention_does_not_inflate_later_push_badges() {
        let (service, receiver) = service_with_push_subscription("phone-badge");

        let removed_runtime = attention_runtime(
            "claude-removed",
            SessionKind::Claude,
            SessionStatus::Running,
        );
        service.push_session_runtime("claude-removed", removed_runtime);
        let removed_key = StableSessionKey::from_tab("claude-removed");
        service.push_semantic_draft(SemanticEventDraft {
            stable_session_key: removed_key.clone(),
            occurred_at_epoch_ms: 20,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::Status {
                state: "completed".to_string(),
                detail: None,
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: Some("removed-completion".to_string()),
        });
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_millis(250))
                .expect("first completion push")
                .payload
                .badge,
            1
        );

        service.push_session_removed("claude-removed");

        let current_runtime = attention_runtime(
            "claude-current",
            SessionKind::Claude,
            SessionStatus::Running,
        );
        service.push_session_runtime("claude-current", current_runtime);
        let current_key = StableSessionKey::from_tab("claude-current");
        service.push_semantic_draft(SemanticEventDraft {
            stable_session_key: current_key,
            occurred_at_epoch_ms: 21,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::Status {
                state: "completed".to_string(),
                detail: None,
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: Some("current-completion".to_string()),
        });

        assert_eq!(
            receiver
                .recv_timeout(Duration::from_millis(250))
                .expect("current completion push")
                .payload
                .badge,
            1,
            "removed session attention must not remain in the aggregate badge"
        );
        assert_eq!(
            service
                .semantic_session_metadata(&removed_key)
                .expect("removed history remains retained")
                .attention,
            SemanticAttention::None
        );
    }

    #[test]
    fn semantic_pushes_require_provider_specific_actionable_states() {
        let (service, receiver) = service_with_push_subscription("phone-provider-status");
        let codex_key = StableSessionKey::from_tab("codex-status");
        let status = |source, key: StableSessionKey, state: &str| SemanticEventDraft {
            stable_session_key: key,
            occurred_at_epoch_ms: 20,
            source,
            kind: SemanticEventKind::Status {
                state: state.to_string(),
                detail: None,
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: None,
        };

        // Codex emits `ready` when a thread starts. That is not a completed
        // turn, and non-AI status strings must never create AI notifications.
        service.push_semantic_draft(status(SemanticSource::Codex, codex_key.clone(), "ready"));
        service.push_semantic_draft(status(
            SemanticSource::Server,
            StableSessionKey::from_server("server-status"),
            "completed",
        ));
        assert!(receiver.try_recv().is_err());

        service.push_semantic_draft(status(SemanticSource::Codex, codex_key, "idle"));
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_millis(250))
                .expect("Codex turn completion push")
                .payload
                .action,
            PushAttentionKind::Completed
        );
    }

    #[test]
    fn native_semantic_adapter_uses_the_existing_journal_store() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let key = StableSessionKey::from_tab("native-claude");

        service.push_semantic_draft(semantic_status_draft(key.clone(), "ready", 42));
        service.push_semantic_adapter_health(key.clone(), SemanticAdapterHealth::Degraded);

        let replay = service.semantic_replay(&key, 0).expect("semantic replay");
        assert_eq!(replay.events.len(), 1);
        assert!(matches!(
            &replay.events[0].kind,
            SemanticEventKind::Status { state, .. } if state == "ready"
        ));
        assert_eq!(
            service
                .semantic_session_metadata(&key)
                .expect("semantic metadata")
                .adapter_health,
            SemanticAdapterHealth::Degraded
        );
    }

    #[test]
    fn push_session_runtime_notifies_subscribed_clients() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (tx, rx) = mpsc::channel();
        if let Ok(mut clients) = service.inner.clients.lock() {
            clients.insert(
                1,
                ConnectedRemoteClient {
                    client_id: "client-1".to_string(),
                    sender: Some(Arc::new(tx)),
                    web_sender: None,
                    web_tombstone: None,
                    semantic_cursors: HashMap::new(),
                    subscribed_session_ids: HashSet::from(["alpha".to_string()]),
                    bootstrapped_session_ids: HashSet::new(),
                    bootstrap_pending_session_ids: HashSet::new(),
                    focused_session_id: Some("alpha".to_string()),
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                    last_snapshot_revision: 0,
                },
            );
        }

        service.push_session_runtime("alpha", session_view("alpha").runtime.clone());

        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ServerMessage::SessionStream {
                event:
                    RemoteSessionStreamEvent::Closed {
                        session_id,
                        runtime,
                    }
                    | RemoteSessionStreamEvent::RuntimePatch {
                        session_id,
                        runtime,
                    },
            }) => {
                assert_eq!(session_id, "alpha");
                assert_eq!(runtime.session_id, "alpha");
            }
            other => panic!("expected runtime stream event, got {other:?}"),
        }
    }

    #[test]
    fn push_session_output_auto_bootstraps_subscribed_client_once_session_is_ready() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (tx, rx) = mpsc::channel();
        if let Ok(mut clients) = service.inner.clients.lock() {
            clients.insert(
                1,
                ConnectedRemoteClient {
                    client_id: "client-1".to_string(),
                    sender: Some(Arc::new(tx)),
                    web_sender: None,
                    web_tombstone: None,
                    semantic_cursors: HashMap::new(),
                    subscribed_session_ids: HashSet::from(["alpha".to_string()]),
                    bootstrapped_session_ids: HashSet::new(),
                    bootstrap_pending_session_ids: HashSet::new(),
                    focused_session_id: Some("alpha".to_string()),
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                    last_snapshot_revision: 0,
                },
            );
        }

        service.push_session_output("alpha", b"before-ready".to_vec());
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ServerMessage::SessionStream {
                event:
                    RemoteSessionStreamEvent::Output {
                        session_id, bytes, ..
                    },
            }) => {
                assert_eq!(session_id, "alpha");
                assert_eq!(bytes, b"before-ready".to_vec());
            }
            other => panic!("expected pre-bootstrap output event, got {other:?}"),
        }

        service.set_session_bootstrap_provider(Some(Arc::new(|session_id| {
            Some(RemoteSessionBootstrap {
                session_id: session_id.to_string(),
                runtime: session_view(session_id).runtime,
                screen: session_view(session_id).screen,
                replay_bytes: format!("{session_id}\r\n").into_bytes(),
            })
        })));

        let mut last_bootstrap_retry_at = HashMap::new();
        deliver_pending_bootstraps(&service.inner, &mut last_bootstrap_retry_at);

        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ServerMessage::SessionStream {
                event: RemoteSessionStreamEvent::Bootstrap { bootstrap },
            }) => assert_eq!(bootstrap.session_id, "alpha"),
            other => panic!("expected late bootstrap event, got {other:?}"),
        }

        service.push_session_output("alpha", b"after-ready".to_vec());

        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ServerMessage::SessionStream {
                event:
                    RemoteSessionStreamEvent::Output {
                        session_id, bytes, ..
                    },
            }) => {
                assert_eq!(session_id, "alpha");
                assert_eq!(bytes, b"after-ready".to_vec());
            }
            other => panic!("expected output event after bootstrap, got {other:?}"),
        }

        {
            let clients = service
                .inner
                .clients
                .lock()
                .expect("client map should be available");
            let client = clients.get(&1).expect("client should remain connected");
            assert!(client.bootstrapped_session_ids.contains("alpha"));
        }
    }

    #[test]
    fn raw_bootstrap_delivery_does_not_publish_semantic_terminal_mode() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let mut app = AppState::default();
        app.open_tabs.push(SessionTab {
            id: "server-tab".to_string(),
            tab_type: TabType::Server,
            project_id: "project-1".to_string(),
            command_id: Some("command-stable".to_string()),
            ..SessionTab::default()
        });
        let mut runtime = SessionRuntimeState::new(
            "pty-runtime",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        runtime.session_kind = SessionKind::Server;
        runtime.command_id = Some("command-stable".to_string());
        let mut runtime_state = RuntimeState::default();
        runtime_state
            .sessions
            .insert(runtime.session_id.clone(), runtime.clone());
        service.update_snapshot(app, runtime_state, HashMap::new());

        let (tx, _rx) = mpsc::channel();
        service.inner.clients.lock().unwrap().insert(
            1,
            ConnectedRemoteClient {
                client_id: "client-1".to_string(),
                sender: Some(Arc::new(tx)),
                web_sender: None,
                web_tombstone: None,
                semantic_cursors: HashMap::new(),
                subscribed_session_ids: HashSet::from(["pty-runtime".to_string()]),
                bootstrapped_session_ids: HashSet::new(),
                bootstrap_pending_session_ids: HashSet::from(["pty-runtime".to_string()]),
                focused_session_id: Some("pty-runtime".to_string()),
                last_app_hash: 0,
                last_runtime_hash: 0,
                last_port_hash: 0,
                last_controller_client_id: None,
                last_you_have_control: false,
                last_snapshot_revision: 0,
            },
        );
        service.set_session_bootstrap_provider(Some(Arc::new(move |_| {
            Some(RemoteSessionBootstrap {
                session_id: "pty-runtime".to_string(),
                runtime: runtime.clone(),
                screen: TerminalScreenSnapshot {
                    mode: TerminalModeSnapshot {
                        alternate_screen: true,
                        ..TerminalModeSnapshot::default()
                    },
                    ..TerminalScreenSnapshot::default()
                },
                replay_bytes: Vec::new(),
            })
        })));
        deliver_pending_bootstraps(&service.inner, &mut HashMap::new());

        let key = StableSessionKey::from_server("command-stable");
        let metadata = service
            .semantic_session_metadata(&key)
            .expect("semantic metadata");
        assert!(!metadata.raw_required);
        let replay = service.semantic_replay(&key, 0).expect("semantic replay");
        assert!(!replay
            .events
            .iter()
            .any(|event| matches!(event.kind, SemanticEventKind::TerminalMode { .. })));
    }

    #[test]
    fn push_session_output_does_not_wait_for_blocked_bootstrap_lookup() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (tx, rx) = mpsc::channel();
        if let Ok(mut clients) = service.inner.clients.lock() {
            clients.insert(
                1,
                ConnectedRemoteClient {
                    client_id: "client-1".to_string(),
                    sender: Some(Arc::new(tx)),
                    web_sender: None,
                    web_tombstone: None,
                    semantic_cursors: HashMap::new(),
                    subscribed_session_ids: HashSet::from(["alpha".to_string()]),
                    bootstrapped_session_ids: HashSet::new(),
                    bootstrap_pending_session_ids: HashSet::new(),
                    focused_session_id: Some("alpha".to_string()),
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                    last_snapshot_revision: 0,
                },
            );
        }

        let release = Arc::new(AtomicBool::new(false));
        let provider_release = release.clone();
        service.set_session_bootstrap_provider(Some(Arc::new(move |_session_id| {
            let started_at = Instant::now();
            while !provider_release.load(Ordering::Relaxed)
                && started_at.elapsed() < Duration::from_secs(1)
            {
                thread::sleep(Duration::from_millis(10));
            }
            Some(RemoteSessionBootstrap {
                session_id: "alpha".to_string(),
                runtime: session_view("alpha").runtime,
                screen: session_view("alpha").screen,
                replay_bytes: b"alpha\r\n".to_vec(),
            })
        })));

        let background = service.clone();
        let join = thread::spawn(move || {
            background.push_session_output("alpha", b"hello".to_vec());
        });

        let output = rx.recv_timeout(Duration::from_millis(250));
        release.store(true, Ordering::Relaxed);
        join.join().expect("push_session_output should return");

        match output {
            Ok(ServerMessage::SessionStream {
                event:
                    RemoteSessionStreamEvent::Output {
                        session_id, bytes, ..
                    },
            }) => {
                assert_eq!(session_id, "alpha");
                assert_eq!(bytes, b"hello".to_vec());
            }
            other => panic!("expected output before bootstrap lookup completes, got {other:?}"),
        }
    }

    #[test]
    fn broadcaster_callback_can_reenter_lifecycle_while_restart_waits_for_it() {
        let _profile = TestProfileGuard::new("reentrant-broadcaster-restart");
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (client_tx, _client_rx) = mpsc::channel();
        let mut client = test_connected_client("client-1", client_tx, None);
        client.subscribed_session_ids.insert("alpha".to_string());
        client
            .bootstrap_pending_session_ids
            .insert("alpha".to_string());
        service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .insert(1, client);

        let (provider_entered_tx, provider_entered_rx) = mpsc::sync_channel(1);
        let (provider_release_tx, provider_release_rx) = mpsc::sync_channel(0);
        let provider_release_rx = Arc::new(Mutex::new(provider_release_rx));
        let (reentry_done_tx, reentry_done_rx) = mpsc::sync_channel(1);
        let reentrant_service = service.clone();
        service.set_session_bootstrap_provider(Some(Arc::new(move |_| {
            provider_entered_tx
                .send(())
                .expect("provider observer should remain");
            provider_release_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv_timeout(Duration::from_secs(3))
                .expect("provider should be released");
            reentrant_service.apply_config(RemoteHostConfig::default());
            reentry_done_tx
                .send(())
                .expect("reentry observer should remain");
            None
        })));

        let broadcaster_inner = Arc::downgrade(&service.inner);
        let broadcaster_signal = service.inner.broadcaster_signal.clone();
        let generation = service
            .inner
            .native_runtime_generation
            .load(Ordering::Acquire);
        let broadcaster = RemoteWorker::spawn("test-reentrant-broadcaster", None, move || {
            run_broadcaster(broadcaster_inner, broadcaster_signal, generation);
        });
        *service
            .inner
            .broadcaster_thread
            .lock()
            .expect("broadcaster handle lock") = Some(broadcaster);
        provider_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("broadcaster should invoke provider");

        let (restart_locked_tx, restart_locked_rx) = mpsc::sync_channel(1);
        *service
            .inner
            .lifecycle_lock_acquired_test_hook
            .write()
            .expect("lifecycle hook lock") = Some(Arc::new(move || {
            let _ = restart_locked_tx.try_send(());
        }));
        let restart_service = service.clone();
        let (restart_done_tx, restart_done_rx) = mpsc::sync_channel(1);
        let restart = thread::spawn(move || {
            restart_service.apply_config(RemoteHostConfig::default());
            restart_done_tx
                .send(())
                .expect("restart observer should remain");
        });
        restart_locked_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("restart should acquire lifecycle state");
        provider_release_tx
            .send(())
            .expect("provider should still be waiting");
        reentry_done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("provider should reenter apply_config without deadlocking");
        restart_done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("restart should finish after provider returns");
        restart.join().expect("restart thread should finish");
    }

    #[test]
    fn broadcaster_bounds_a_blocked_bootstrap_callback_and_reports_residue() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (client_tx, _client_rx) = mpsc::channel();
        let mut client = test_connected_client("client-1", client_tx, None);
        client.subscribed_session_ids.insert("alpha".to_string());
        client
            .bootstrap_pending_session_ids
            .insert("alpha".to_string());
        service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .insert(1, client);

        let (provider_entered_tx, provider_entered_rx) = mpsc::sync_channel(1);
        let (provider_release_tx, provider_release_rx) = mpsc::sync_channel(0);
        let provider_release_rx = Arc::new(Mutex::new(provider_release_rx));
        service.set_session_bootstrap_provider(Some(Arc::new(move |_| {
            provider_entered_tx
                .send(())
                .expect("provider observer should remain");
            provider_release_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv_timeout(Duration::from_secs(3))
                .expect("provider should be released");
            None
        })));

        let delivery_inner = service.inner.clone();
        let (delivery_done_tx, delivery_done_rx) = mpsc::sync_channel(1);
        let delivery = thread::spawn(move || {
            deliver_pending_bootstraps(&delivery_inner, &mut HashMap::new());
            delivery_done_tx
                .send(())
                .expect("delivery observer should remain");
        });
        provider_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("delivery should invoke provider");
        let returned_within_bound = delivery_done_rx
            .recv_timeout(Duration::from_secs(1))
            .is_ok();
        let bounded_status = returned_within_bound.then(|| service.status());

        provider_release_tx
            .send(())
            .expect("provider should still be waiting");
        if !returned_within_bound {
            delivery_done_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("delivery should finish once provider is released");
        }
        delivery.join().expect("delivery worker should finish");

        assert!(
            returned_within_bound,
            "broadcaster waited indefinitely for a bootstrap callback"
        );
        let status = bounded_status.expect("bounded callback should expose status");
        assert!(status.last_connection_is_error);
        assert!(
            status
                .last_connection_note
                .as_deref()
                .is_some_and(|note| note.contains("worker residue")),
            "callback timeout did not expose retained worker ownership: {:?}",
            status.last_connection_note
        );
    }

    #[test]
    fn owner_drop_is_bounded_and_reports_a_blocked_broadcaster_residue() {
        let _profile = TestProfileGuard::new("bounded-broadcaster-shutdown");
        let root = RemoteHostService::new(RemoteHostConfig::default());
        let observer = root.clone();
        let (worker_reaped_tx, worker_reaped_rx) = mpsc::sync_channel(1);
        *root
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = Some(worker_reaped_tx);
        let (broadcaster_entered_tx, broadcaster_entered_rx) = mpsc::sync_channel(1);
        let (broadcaster_release_tx, broadcaster_release_rx) = mpsc::sync_channel(0);
        let broadcaster = RemoteWorker::spawn("test-blocked-broadcaster", None, move || {
            broadcaster_entered_tx
                .send(())
                .expect("broadcaster observer should remain");
            broadcaster_release_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("broadcaster should be released");
        });
        *root
            .inner
            .broadcaster_thread
            .lock()
            .expect("broadcaster handle lock") = Some(broadcaster);
        broadcaster_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("broadcaster should start");

        let (drop_done_tx, drop_done_rx) = mpsc::sync_channel(1);
        let drop_thread = thread::spawn(move || {
            drop(root);
            drop_done_tx.send(()).expect("drop observer should remain");
        });
        let returned_within_bound = drop_done_rx.recv_timeout(Duration::from_secs(1)).is_ok();
        if returned_within_bound {
            let status = observer.status();
            assert!(status.last_connection_is_error);
            assert!(
                status
                    .last_connection_note
                    .as_deref()
                    .is_some_and(|note| note.contains("worker residue")),
                "bounded shutdown did not expose its residue: {:?}",
                status.last_connection_note
            );
        }
        broadcaster_release_tx
            .send(())
            .expect("broadcaster should still be waiting");
        if !returned_within_bound {
            drop_done_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("drop should finish once the broadcaster is released");
        }
        drop_thread.join().expect("drop thread should finish");
        let reaped_worker = worker_reaped_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("retained broadcaster handle should be joined after callback exit");

        assert!(
            returned_within_bound,
            "owner drop waited indefinitely for a blocked broadcaster worker"
        );
        assert_eq!(reaped_worker.name, "test-blocked-broadcaster");
        *observer
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = None;
        assert_eq!(
            observer.inner.worker_residue_count.load(Ordering::Acquire),
            0,
            "joined deferred worker remained reported as residue"
        );
    }

    #[test]
    fn deferred_worker_reaping_keeps_observation_bound_to_defer_generation() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (blocked_entered_tx, blocked_entered_rx) = mpsc::sync_channel(1);
        let (blocked_release_tx, blocked_release_rx) = mpsc::sync_channel(0);
        let blocked = RemoteWorker::spawn("test-generation-scoped-reaper", None, move || {
            blocked_entered_tx
                .send(())
                .expect("blocked observer should remain");
            blocked_release_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("blocked worker should be released");
        });
        blocked_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("blocked worker should start");

        // The worker is already deferred before this observer is installed.
        // A later test hook must not receive an event for an older worker.
        super::defer_remote_worker(&service.inner, blocked);
        let (reaped_tx, reaped_rx) = mpsc::sync_channel(1);
        *service
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = Some(reaped_tx);

        blocked_release_tx
            .send(())
            .expect("blocked worker should still be waiting");
        assert!(
            reaped_rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "a hook installed after deferral observed an older worker"
        );
        *service
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = None;
        drop(service);
    }

    #[test]
    fn deferred_worker_reaper_joins_completed_work_behind_a_blocked_worker() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (reaped_tx, reaped_rx) = mpsc::sync_channel(2);
        *service
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = Some(reaped_tx);

        let (blocked_entered_tx, blocked_entered_rx) = mpsc::sync_channel(1);
        let (blocked_release_tx, blocked_release_rx) = mpsc::sync_channel(0);
        let blocked = RemoteWorker::spawn("test-reaper-blocked", None, move || {
            blocked_entered_tx
                .send(())
                .expect("blocked observer should remain");
            blocked_release_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("blocked worker should be released");
        });
        blocked_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("blocked worker should start");
        super::defer_remote_worker(&service.inner, blocked);

        let completed = RemoteWorker::spawn("test-reaper-completed", None, || {});
        super::defer_remote_worker(&service.inner, completed);
        let first_reaped = reaped_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("completed deferred worker should be joined independently");
        assert_eq!(
            first_reaped.name, "test-reaper-completed",
            "a blocked deferred worker prevented the reaper from joining independent completed work"
        );
        blocked_release_tx
            .send(())
            .expect("blocked worker should still be waiting");
        let second_reaped = reaped_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("released deferred worker should eventually be joined");
        assert_eq!(
            second_reaped.name, "test-reaper-blocked",
            "the released deferred worker should be reaped after its completion"
        );
        *service
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = None;
    }

    #[test]
    fn restarting_reaper_joins_prior_handle_before_replacement() {
        let (sender, receiver) = mpsc::sync_channel::<DeferredRemoteWorker>(1);
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let prior_handle = thread::spawn(move || {
            entered_tx
                .send(())
                .expect("prior reaper should report that it started");
            release_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("prior reaper should be released");
            drop(receiver);
        });
        entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("prior reaper should start before restart");

        let reaper = Arc::new(RemoteWorkerReaper {
            sender: Mutex::new(Some(sender)),
            handle: Mutex::new(Some(prior_handle)),
            fallback: Arc::new(Mutex::new(VecDeque::new())),
            signal: remote_worker_reaper_signal().clone(),
            lifecycle: Mutex::new(()),
        });
        let (restart_done_tx, restart_done_rx) = mpsc::sync_channel(1);
        let restart_reaper = Arc::clone(&reaper);
        let restart_thread = thread::spawn(move || {
            let result = restart_reaper.restart();
            restart_done_tx
                .send(result)
                .expect("restart result receiver should remain available");
        });

        let first_restart_result = restart_done_rx.recv_timeout(Duration::from_millis(250));
        let returned_before_prior_exit = first_restart_result.is_ok();
        release_tx
            .send(())
            .expect("prior reaper should be released for the join");
        let restart_result = match first_restart_result {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => restart_done_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("restart should finish after the prior reaper exits"),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("restart thread exited without reporting a result")
            }
        };
        assert!(
            restart_result.is_ok(),
            "reaper restart failed: {restart_result:?}"
        );
        restart_thread
            .join()
            .expect("restart thread should join cleanly");

        drop(reaper);
        assert!(
            !returned_before_prior_exit,
            "restart replaced and discarded the prior reaper before joining it"
        );
    }

    #[test]
    fn bounded_reaper_admission_reports_full_without_blocking() {
        let (sender, _receiver) = mpsc::sync_channel::<DeferredRemoteWorker>(0);
        let reaper = RemoteWorkerReaper {
            sender: Mutex::new(Some(sender)),
            handle: Mutex::new(None),
            fallback: Arc::new(Mutex::new(VecDeque::new())),
            signal: remote_worker_reaper_signal().clone(),
            lifecycle: Mutex::new(()),
        };
        let mut worker = RemoteWorker::spawn("test-full-reaper-admission", None, || {});
        let handle = worker.handle.take().expect("worker handle");
        let admission = reaper.send(DeferredRemoteWorker {
            name: worker.name,
            generation: 0,
            handle,
            owner: DeferredRemoteWorkerOwner::Unowned,
            reap_observer: None,
        });
        let worker = match admission {
            DeferredRemoteWorkerAdmission::Full(worker) => worker,
            DeferredRemoteWorkerAdmission::Accepted => {
                panic!("zero-capacity reaper admission unexpectedly accepted a worker")
            }
            DeferredRemoteWorkerAdmission::Closed(worker)
            | DeferredRemoteWorkerAdmission::Unavailable(worker) => {
                finish_deferred_remote_worker(worker);
                panic!("zero-capacity reaper admission did not report Full")
            }
        };
        finish_deferred_remote_worker(worker);
    }

    #[test]
    fn remote_worker_admission_rejects_third_before_thread_starts() {
        let pool = Arc::new(RemoteWorkerAdmissionPool::new(2));
        let (started_tx, started_rx) = mpsc::sync_channel(2);
        let (first_release_tx, first_release_rx) = mpsc::sync_channel(0);
        let (second_release_tx, second_release_rx) = mpsc::sync_channel(0);
        let first =
            RemoteWorker::try_spawn_with_pool(Arc::clone(&pool), "test-admission-first", None, {
                let started_tx = started_tx.clone();
                move || {
                    started_tx.send(()).expect("first worker should start");
                    first_release_rx
                        .recv()
                        .expect("first worker should be released");
                }
            })
            .expect("first worker should be admitted");
        let second =
            RemoteWorker::try_spawn_with_pool(Arc::clone(&pool), "test-admission-second", None, {
                let started_tx = started_tx.clone();
                move || {
                    started_tx.send(()).expect("second worker should start");
                    second_release_rx
                        .recv()
                        .expect("second worker should be released");
                }
            })
            .expect("second worker should be admitted");
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first worker should report its start");
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("second worker should report its start");

        let third_started = Arc::new(AtomicUsize::new(0));
        let third_started_job = Arc::clone(&third_started);
        let third = RemoteWorker::try_spawn_with_pool(
            Arc::clone(&pool),
            "test-admission-third",
            None,
            move || {
                third_started_job.fetch_add(1, Ordering::AcqRel);
            },
        );
        assert!(matches!(
            third,
            Err(RemoteWorkerSpawnError::AdmissionUnavailable { .. })
        ));
        assert_eq!(third_started.load(Ordering::Acquire), 0);
        assert_eq!(pool.in_use(), 2);

        first_release_tx.send(()).expect("first worker release");
        second_release_tx.send(()).expect("second worker release");
        first
            .handle
            .expect("first worker handle")
            .join()
            .expect("first worker should join");
        second
            .handle
            .expect("second worker handle")
            .join()
            .expect("second worker should join");
        assert_eq!(pool.in_use(), 0);
    }

    #[test]
    fn full_fallback_retains_the_65th_worker_without_synchronous_join() {
        let pool = Arc::new(RemoteWorkerAdmissionPool::new(65));
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let release_rx = Arc::new(Mutex::new(release_rx));
        let mut workers = Vec::new();
        for index in 0..65 {
            let release_rx = Arc::clone(&release_rx);
            workers.push(
                RemoteWorker::try_spawn_with_pool(
                    Arc::clone(&pool),
                    format!("test-fallback-{index}"),
                    None,
                    move || {
                        release_rx
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .recv()
                            .expect("fallback worker should be released");
                    },
                )
                .expect("fallback worker should be admitted"),
            );
        }
        let reaper = Arc::new(RemoteWorkerReaper {
            sender: Mutex::new(None),
            handle: Mutex::new(None),
            fallback: Arc::new(Mutex::new(VecDeque::new())),
            signal: remote_worker_reaper_signal().clone(),
            lifecycle: Mutex::new(()),
        });
        for worker in workers.drain(..64) {
            let handle = worker.handle.expect("fallback worker handle");
            reaper.retain_after_failure(
                DeferredRemoteWorker {
                    name: worker.name,
                    generation: 0,
                    handle,
                    owner: DeferredRemoteWorkerOwner::Unowned,
                    reap_observer: None,
                },
                "test fallback capacity",
            );
        }
        let last_worker = workers
            .pop()
            .expect("65th fallback worker should remain available");
        let last_handle = last_worker.handle.expect("65th fallback worker handle");
        let last_deferred = DeferredRemoteWorker {
            name: last_worker.name,
            generation: 0,
            handle: last_handle,
            owner: DeferredRemoteWorkerOwner::Unowned,
            reap_observer: None,
        };
        let (retained_tx, retained_rx) = mpsc::sync_channel(1);
        let retained_reaper = Arc::clone(&reaper);
        let retain_thread = thread::spawn(move || {
            retained_reaper.retain_after_failure(last_deferred, "test 65th fallback");
            retained_tx
                .send(())
                .expect("retention should complete without joining");
        });
        retained_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("65th fallback retention must not synchronously join");
        assert_eq!(
            reaper
                .fallback
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            65,
            "the 65th admitted worker must remain owned by fallback"
        );
        for _ in 0..65 {
            release_tx.send(()).expect("fallback worker release");
        }
        retain_thread.join().expect("retention thread should join");
        drop(reaper);
        assert_eq!(pool.in_use(), 0, "fallback joins must release every permit");
    }

    #[test]
    fn remote_worker_permit_returns_only_after_join() {
        let pool = Arc::new(RemoteWorkerAdmissionPool::new(1));
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let worker = RemoteWorker::try_spawn_with_pool(
            Arc::clone(&pool),
            "test-permit-lifetime",
            None,
            move || {
                entered_tx.send(()).expect("worker should enter");
                release_rx.recv().expect("worker should be released");
            },
        )
        .expect("worker should be admitted");
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker should enter before lifetime assertions");
        let handle = worker.handle.expect("worker handle");
        assert_eq!(pool.in_use(), 1);
        release_tx.send(()).expect("worker release");
        assert_eq!(pool.in_use(), 1, "completion must not release before join");
        handle.join().expect("worker should join");
        assert_eq!(pool.in_use(), 0, "join must release the permit");
    }

    #[test]
    fn unavailable_reaper_finishes_residue_without_detached_worker() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (reaped_tx, reaped_rx) = mpsc::sync_channel(1);
        *service
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = Some(reaped_tx.clone());

        let mut worker = RemoteWorker::spawn("test-unavailable-reaper", None, || {});
        let handle = worker.handle.take().expect("worker handle");
        service
            .inner
            .worker_residue_count
            .fetch_add(1, Ordering::AcqRel);
        set_last_connection_note(
            &service.inner,
            "Remote worker residue: test-unavailable-reaper".to_string(),
            true,
        );
        let reaper = RemoteWorkerReaper {
            sender: Mutex::new(None),
            handle: Mutex::new(None),
            fallback: Arc::new(Mutex::new(VecDeque::new())),
            signal: remote_worker_reaper_signal().clone(),
            lifecycle: Mutex::new(()),
        };
        let worker = match reaper.send(DeferredRemoteWorker {
            name: worker.name,
            generation: service
                .inner
                .native_runtime_generation
                .load(Ordering::Acquire),
            handle,
            owner: DeferredRemoteWorkerOwner::Host(Arc::downgrade(&service.inner)),
            reap_observer: Some(reaped_tx),
        }) {
            DeferredRemoteWorkerAdmission::Unavailable(worker) => worker,
            DeferredRemoteWorkerAdmission::Accepted
            | DeferredRemoteWorkerAdmission::Full(_)
            | DeferredRemoteWorkerAdmission::Closed(_) => {
                panic!("inert reaper did not report an unavailable admission")
            }
        };
        reaper.retain_after_failure(worker, "test reaper startup failure");
        assert_eq!(
            reaper
                .fallback
                .lock()
                .expect("fallback registry lock")
                .len(),
            1,
            "unavailable reaper must retain the worker without joining inline"
        );
        assert!(
            reaped_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "unavailable reaper joined residue synchronously"
        );
        drop(reaper);
        let reaped = reaped_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("unavailable reaper fallback must be joined during drop");
        assert_eq!(reaped.name, "test-unavailable-reaper");
        assert_eq!(
            service.inner.worker_residue_count.load(Ordering::Acquire),
            0,
            "unavailable reaper path left residue unjoined"
        );
        *service
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = None;
    }

    #[test]
    fn full_reaper_admission_retains_worker_in_owned_bounded_registry() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (reaped_tx, reaped_rx) = mpsc::sync_channel(1);
        *service
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = Some(reaped_tx.clone());

        let mut worker = RemoteWorker::spawn("test-full-reaper-residue", None, || {});
        let handle = worker.handle.take().expect("worker handle");
        service
            .inner
            .worker_residue_count
            .fetch_add(1, Ordering::AcqRel);
        set_last_connection_note(
            &service.inner,
            "Remote worker residue: test-full-reaper-residue".to_string(),
            true,
        );

        let (sender, receiver) = mpsc::sync_channel::<DeferredRemoteWorker>(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let reaper_handle = thread::spawn(move || {
            release_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("test reaper owner should be released");
        });
        let reaper = RemoteWorkerReaper {
            sender: Mutex::new(Some(sender)),
            handle: Mutex::new(Some(reaper_handle)),
            fallback: Arc::new(Mutex::new(VecDeque::new())),
            signal: remote_worker_reaper_signal().clone(),
            lifecycle: Mutex::new(()),
        };

        enqueue_deferred_remote_worker_with_reaper(
            &reaper,
            DeferredRemoteWorker {
                name: worker.name,
                generation: service
                    .inner
                    .native_runtime_generation
                    .load(Ordering::Acquire),
                handle,
                owner: DeferredRemoteWorkerOwner::Host(Arc::downgrade(&service.inner)),
                reap_observer: Some(reaped_tx),
            },
        );
        assert_eq!(
            reaper
                .fallback
                .lock()
                .expect("fallback registry lock")
                .len(),
            1,
            "Full admission must retain the worker in the bounded owner registry"
        );
        assert!(
            reaped_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "a fallback worker was joined outside the owned registry"
        );

        drop(receiver);
        release_tx
            .send(())
            .expect("owned fallback reaper should be released");
        drop(reaper);
        let reaped = reaped_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("owned fallback worker should be joined during reaper drop");
        assert_eq!(reaped.name, "test-full-reaper-residue");
        assert_eq!(
            service.inner.worker_residue_count.load(Ordering::Acquire),
            0,
            "owned fallback worker remained reported as residue after joining"
        );
        *service
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = None;
    }

    #[test]
    fn closed_deferred_worker_reaper_channel_restarts_without_losing_residue() {
        // Exercise the restart-owned reaper while the process-global reaper
        // is also waiting on the shared event signal.
        let _global_reaper = super::remote_worker_reaper();
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (reaped_tx, reaped_rx) = mpsc::sync_channel(1);
        *service
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = Some(reaped_tx.clone());

        let mut worker = RemoteWorker::spawn("test-closed-reaper", None, || {});
        let handle = worker.handle.take().expect("worker handle");
        service
            .inner
            .worker_residue_count
            .fetch_add(1, Ordering::AcqRel);
        set_last_connection_note(
            &service.inner,
            "Remote worker residue: test-closed-reaper".to_string(),
            true,
        );

        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let reaper = RemoteWorkerReaper {
            sender: Mutex::new(Some(sender)),
            handle: Mutex::new(None),
            fallback: Arc::new(Mutex::new(VecDeque::new())),
            signal: remote_worker_reaper_signal().clone(),
            lifecycle: Mutex::new(()),
        };
        enqueue_deferred_remote_worker_with_reaper(
            &reaper,
            DeferredRemoteWorker {
                name: worker.name,
                generation: service
                    .inner
                    .native_runtime_generation
                    .load(Ordering::Acquire),
                handle,
                owner: DeferredRemoteWorkerOwner::Host(Arc::downgrade(&service.inner)),
                reap_observer: Some(reaped_tx),
            },
        );

        let reaped = reaped_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("worker should be reaped after the channel restart");
        assert_eq!(reaped.name, "test-closed-reaper");
        assert_eq!(
            service.inner.worker_residue_count.load(Ordering::Acquire),
            0,
            "channel closure must not lose the retained worker"
        );
        *service
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = None;
        drop(reaper);
    }

    #[test]
    fn update_snapshot_parts_only_replaces_changed_sections() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let mut app_state = AppState::default();
        app_state.sidebar_collapsed = true;
        let runtime_state = RuntimeState::default();
        let port_statuses = HashMap::from([(
            3000,
            PortStatus {
                port: 3000,
                in_use: true,
                pid: Some(42),
                process_name: Some("node".to_string()),
            },
        )]);
        service.update_snapshot(
            app_state.clone(),
            runtime_state.clone(),
            port_statuses.clone(),
        );
        assert!(
            service
                .inner
                .port_authorities
                .read()
                .expect("port authority lock")
                .is_empty(),
            "legacy snapshot publication must fail closed instead of retaining a stale typed fence"
        );

        let mut next_runtime = runtime_state;
        next_runtime.active_session_id = Some("server-session".to_string());

        let before_revision = service.inner.snapshot_revision.load(Ordering::Relaxed);
        service.update_snapshot_parts(None, Some(next_runtime.clone()), None, None);

        let stored_app = service
            .inner
            .shared_state
            .read()
            .expect("shared state lock")
            .clone();
        let stored_runtime = service
            .inner
            .runtime_state
            .read()
            .expect("runtime state lock")
            .clone();
        let stored_ports = service
            .inner
            .port_statuses
            .read()
            .expect("port statuses lock")
            .clone();

        assert!(stored_app.sidebar_collapsed);
        assert_eq!(
            stored_runtime.active_session_id,
            next_runtime.active_session_id
        );
        assert_eq!(stored_ports, port_statuses);
        assert!(service.inner.snapshot_revision.load(Ordering::Relaxed) > before_revision);
    }

    #[test]
    fn remote_projection_preserves_managed_unready_health() {
        let status = crate::process::ports::PortStatus {
            port: 43123,
            resource: ResourceFence::new(crate::domain::id::ResourceId::new(), 7),
            kind: crate::process::ports::PortStatusKind::ManagedUnready,
            listeners: Arc::from([]),
            error: None,
        };
        let authority = RemotePortAuthority::from_rich(&status, now_epoch_ms());

        assert_eq!(authority.kind(), RemotePortAuthorityKind::ManagedUnready);
        assert_eq!(authority.resource, Some(status.resource));
    }

    #[test]
    fn remote_probe_error_is_typed_and_does_not_export_raw_detail() {
        let status = crate::process::ports::PortStatus {
            port: 43124,
            resource: ResourceFence::new(crate::domain::id::ResourceId::new(), 7),
            kind: crate::process::ports::PortStatusKind::ProbeError,
            listeners: Arc::from([]),
            error: Some("C:\\private\\listener-table.txt".to_string()),
        };
        let authority = RemotePortAuthority::from_rich(&status, now_epoch_ms());

        assert_eq!(authority.kind(), RemotePortAuthorityKind::ProbeError);
        assert_eq!(
            authority.diagnostic,
            Some(super::RemotePortDiagnostic::ProbeError)
        );
        assert_eq!(authority.error, None);
        let wire = serde_json::to_string(&authority).expect("serialize remote authority");
        assert!(wire.contains("probeError"));
        assert!(!wire.contains("listener-table.txt"));
    }

    #[test]
    fn remote_probe_error_preserves_source_observation_window() {
        let status = crate::process::ports::PortStatus {
            port: 43126,
            resource: ResourceFence::new(crate::domain::id::ResourceId::new(), 7),
            kind: crate::process::ports::PortStatusKind::ProbeError,
            listeners: Arc::from([]),
            error: Some("listener probe failed".to_string()),
        };
        let authority =
            RemotePortAuthority::from_rich_with_source_metadata(&status, 20_000, 21_000);

        assert_eq!(authority.observed_at_epoch_ms, 20_000);
        assert_eq!(authority.freshness_deadline_epoch_ms, 21_000);
        assert!(!authority.is_fresh_at(21_001));
    }

    #[test]
    fn remote_rejects_proven_external_probe_diagnostic() {
        let status = crate::process::ports::PortStatus {
            port: 43127,
            resource: ResourceFence::new(crate::domain::id::ResourceId::new(), 7),
            kind: crate::process::ports::PortStatusKind::ProvenExternal,
            listeners: Arc::from([]),
            error: Some("listener probe failed".to_string()),
        };
        let authority = RemotePortAuthority::from_rich(&status, now_epoch_ms());

        assert_eq!(authority.kind(), RemotePortAuthorityKind::Unknown);
        assert_eq!(
            authority.diagnostic,
            Some(super::RemotePortDiagnostic::ProbeError)
        );
    }

    #[test]
    fn remote_starting_probe_detail_is_typed_and_does_not_export_raw_detail() {
        let status = crate::process::ports::PortStatus {
            port: 43125,
            resource: ResourceFence::new(crate::domain::id::ResourceId::new(), 8),
            kind: crate::process::ports::PortStatusKind::Starting,
            listeners: Arc::from([]),
            error: Some("C:\\private\\secret-startup-token.txt".to_string()),
        };
        let authority = RemotePortAuthority::from_rich(&status, now_epoch_ms());

        assert_eq!(authority.kind(), RemotePortAuthorityKind::Unknown);
        assert_eq!(
            authority.diagnostic,
            Some(super::RemotePortDiagnostic::ProbeError)
        );
        assert_eq!(authority.error, None);
        let wire = serde_json::to_string(&authority).expect("serialize remote authority");
        assert!(wire.contains("probeError"));
        assert!(!wire.contains("secret-startup-token.txt"));
    }

    #[test]
    fn legacy_pid_only_port_status_cannot_prove_remote_forward_authority() {
        let mut session = SessionRuntimeState::new(
            "remote-port-authority",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.status = SessionStatus::Running;
        session.pid = Some(4242);
        let now = super::now_epoch_ms();
        let authority = RemotePortAuthority {
            port: 43123,
            kind: RemotePortAuthorityKind::Unknown,
            diagnostic: None,
            resource: None,
            listeners: Vec::new(),
            session_id: None,
            root: None,
            membership_revision: 0,
            observation_sequence: 0,
            publication_sequence: 0,
            observed_at_epoch_ms: now,
            freshness_deadline_epoch_ms: now,
            managed_fence_fingerprint: None,
            verified: None,
            error: Some("legacy PID-only status".to_string()),
        };

        assert!(!super::remote_authority_allows_forward(
            &authority, 43123, &session, now
        ));
    }

    #[test]
    fn exact_remote_authority_fence_allows_forwarding() {
        let mut session = SessionRuntimeState::new(
            "remote-port-authority",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.status = SessionStatus::Running;
        session.pid = Some(4242);
        let now = super::now_epoch_ms();
        let authority = RemotePortAuthority {
            port: 43123,
            kind: RemotePortAuthorityKind::Managed,
            diagnostic: None,
            resource: Some(ResourceFence::new(crate::domain::id::ResourceId::new(), 7)),
            listeners: vec![RemoteListenerIdentity {
                pid: 4242,
                creation_time_100ns: 42_420_000,
                executable_proven: true,
                executable_fingerprint: None,
            }],
            session_id: None,
            root: None,
            membership_revision: 9,
            observation_sequence: 11,
            publication_sequence: 13,
            observed_at_epoch_ms: now,
            freshness_deadline_epoch_ms: now + REMOTE_PORT_AUTHORITY_MAX_AGE_MS,
            managed_fence_fingerprint: None,
            verified: None,
            error: None,
        };

        assert!(!super::remote_authority_allows_forward(
            &authority, 43123, &session, now
        ));
        assert!(!super::remote_authority_allows_forward(
            &authority, 43124, &session, now
        ));
    }

    #[test]
    fn exact_remote_authority_requires_the_injected_live_registry_fence() {
        let mut session = SessionRuntimeState::new(
            "remote-port-authority",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.status = SessionStatus::Running;
        session.pid = Some(4242);
        session.server_launch = Some(crate::state::ServerLaunchSpec {
            command_id: "server-command".to_string(),
            project_id: "server-project".to_string(),
            port: Some(43123),
            cwd: PathBuf::new(),
            program: "test-server".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            auto_restart: false,
            log_file_path: None,
        });
        let executable = std::env::current_exe().expect("test executable");
        let root = ManagedProcessIdentity::new(
            ManagedProcessId::new(4242, 42_420_000).unwrap(),
            executable,
        )
        .unwrap();
        let resource = ResourceFence::new(crate::domain::id::ResourceId::new(), 7);
        let live = Arc::new(test_capability_from_snapshot(ManagedResourceSnapshot::new(
            crate::process::registry::ManagedProcessFence::new(
                resource,
                ProcessOwner::Host,
                root.clone(),
            ),
            ManagedProcessState::Running,
            vec![root],
            RegistryMembershipSnapshot::valid(9, 11, Instant::now(), Duration::from_secs(5)),
        )));
        let now = super::now_epoch_ms();
        let authority = RemotePortAuthority {
            port: 43123,
            kind: RemotePortAuthorityKind::Managed,
            diagnostic: None,
            resource: Some(resource),
            listeners: vec![RemoteListenerIdentity {
                pid: 4242,
                creation_time_100ns: 42_420_000,
                executable_proven: true,
                executable_fingerprint: None,
            }],
            session_id: Some(session.session_id.clone()),
            root: Some(RemoteListenerIdentity {
                pid: 4242,
                creation_time_100ns: 42_420_000,
                executable_proven: true,
                executable_fingerprint: None,
            }),
            membership_revision: 9,
            observation_sequence: 11,
            publication_sequence: 13,
            observed_at_epoch_ms: now,
            freshness_deadline_epoch_ms: now + REMOTE_PORT_AUTHORITY_MAX_AGE_MS,
            managed_fence_fingerprint: Some(live.snapshot().authority_fingerprint()),
            verified: None,
            error: None,
        }
        .with_managed_capability(live.as_ref());

        let mut runtime = RuntimeState::default();
        runtime
            .sessions
            .insert("server-command".to_string(), session.clone());
        let observation_time = Instant::now();
        let projected = super::host_verified_port_authorities_at(
            &HashMap::from([(43123, authority.clone())]),
            &runtime,
            &HashMap::from([(43123, live.clone())]),
            now,
            observation_time,
            observation_time + Duration::from_secs(5),
        );
        assert!(
            projected
                .get(&43123)
                .is_some_and(RemotePortAuthority::is_host_verified),
            "only the host correlation seam may mint the authority marker"
        );
        let mut forged_projected = projected.get(&43123).expect("projected authority").clone();
        forged_projected.port = 43124;
        assert!(
            !forged_projected.is_host_verified(),
            "mutating a verified projection must invalidate its private proof"
        );
        let wire_round_trip: RemotePortAuthority = serde_json::from_slice(
            &serde_json::to_vec(projected.get(&43123).expect("projected authority"))
                .expect("serialize projected authority"),
        )
        .expect("deserialize projected authority");
        assert!(!wire_round_trip.is_host_verified());

        assert!(super::remote_authority_allows_forward_with_live(
            &authority,
            43123,
            &session,
            now,
            Some(live.as_ref()),
        ));
        let mut unready = authority.clone();
        unready.kind = RemotePortAuthorityKind::ManagedUnready;
        assert!(
            !super::remote_authority_allows_forward_with_live(
                &unready,
                43123,
                &session,
                now,
                Some(live.as_ref()),
            ),
            "managed-unready authority may describe health but cannot authorize forwarding"
        );
        let mut missing_session = authority.clone();
        missing_session.session_id = None;
        assert!(!super::remote_authority_allows_forward_with_live(
            &missing_session,
            43123,
            &session,
            now,
            Some(live.as_ref()),
        ));
        let mut forged_session = authority.clone();
        forged_session.session_id = Some("forged-session-id".to_string());
        assert!(!super::remote_authority_allows_forward_with_live(
            &forged_session,
            43123,
            &session,
            now,
            Some(live.as_ref()),
        ));
        let mut wrong_root = authority.clone();
        wrong_root.root.as_mut().expect("root identity").pid += 1;
        assert!(!super::remote_authority_allows_forward_with_live(
            &wrong_root,
            43123,
            &session,
            now,
            Some(live.as_ref()),
        ));
        let mut shape_only = authority.clone();
        shape_only.managed_fence_fingerprint =
            Some(live.snapshot().authority_fingerprint().wrapping_add(1));
        assert!(!super::remote_authority_allows_forward_with_live(
            &shape_only,
            43123,
            &session,
            now,
            Some(live.as_ref()),
        ));
        let mut errored = authority.clone();
        errored.error = Some("membership probe failed".to_string());
        assert!(!super::remote_authority_allows_forward_with_live(
            &errored,
            43123,
            &session,
            now,
            Some(live.as_ref()),
        ));
        let mut forged_listener_executable = authority.clone();
        forged_listener_executable.listeners[0].executable_fingerprint = Some(
            forged_listener_executable.listeners[0]
                .executable_fingerprint
                .expect("listener executable fingerprint")
                .wrapping_add(1),
        );
        assert!(!super::remote_authority_allows_forward_with_live(
            &forged_listener_executable,
            43123,
            &session,
            now,
            Some(live.as_ref()),
        ));
        let mut forged_member = authority.clone();
        forged_member.listeners[0].pid += 1;
        assert!(!super::remote_authority_allows_forward_with_live(
            &forged_member,
            43123,
            &session,
            now,
            Some(live.as_ref()),
        ));
        let mut wrong_session_port = session.clone();
        wrong_session_port
            .server_launch
            .as_mut()
            .expect("server launch")
            .port = Some(43124);
        assert!(!super::remote_authority_allows_forward_with_live(
            &authority,
            43123,
            &wrong_session_port,
            now,
            Some(live.as_ref()),
        ));
        let observation_time = Instant::now();
        assert!(!authority.has_exact_managed_fence_for(
            43123,
            &session,
            &live,
            now,
            observation_time,
            observation_time - Duration::from_millis(1),
        ));
    }

    #[test]
    fn stale_or_pid_only_remote_authority_cannot_forward() {
        let mut session = SessionRuntimeState::new(
            "remote-port-authority",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.status = SessionStatus::Running;
        session.pid = Some(4242);
        let now = super::now_epoch_ms();
        let mut authority = RemotePortAuthority {
            port: 43123,
            kind: RemotePortAuthorityKind::Managed,
            diagnostic: None,
            resource: Some(ResourceFence::new(crate::domain::id::ResourceId::new(), 7)),
            listeners: vec![RemoteListenerIdentity {
                pid: 4242,
                creation_time_100ns: 42_420_000,
                executable_proven: true,
                executable_fingerprint: None,
            }],
            session_id: None,
            root: None,
            membership_revision: 9,
            observation_sequence: 11,
            publication_sequence: 13,
            observed_at_epoch_ms: now.saturating_sub(REMOTE_PORT_AUTHORITY_MAX_AGE_MS + 1),
            freshness_deadline_epoch_ms: now.saturating_sub(1),
            managed_fence_fingerprint: None,
            verified: None,
            error: None,
        };
        assert!(!super::remote_authority_allows_forward(
            &authority, 43123, &session, now
        ));

        authority.observed_at_epoch_ms = now;
        authority.freshness_deadline_epoch_ms = now + REMOTE_PORT_AUTHORITY_MAX_AGE_MS;
        authority.listeners[0].executable_proven = false;
        assert!(!super::remote_authority_allows_forward(
            &authority, 43123, &session, now
        ));

        authority.listeners[0].executable_proven = true;
        authority.resource = Some(ResourceFence::new(crate::domain::id::ResourceId::new(), 0));
        assert!(!super::remote_authority_allows_forward(
            &authority, 43123, &session, now
        ));
    }

    #[test]
    fn update_snapshot_parts_ignores_empty_updates() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let before_revision = service.inner.snapshot_revision.load(Ordering::Relaxed);

        service.update_snapshot_parts(None, None, None, None);

        assert_eq!(
            service.inner.snapshot_revision.load(Ordering::Relaxed),
            before_revision
        );
    }

    #[test]
    fn remote_delta_carries_typed_port_authorities_alongside_legacy_statuses() {
        let delta = RemoteWorkspaceDelta {
            app_state: None,
            runtime_state: None,
            port_statuses: Some(HashMap::new()),
            port_authorities: None,
            controller_client_id: None,
            you_have_control: false,
        };
        let encoded = serde_json::to_value(delta).expect("remote delta should serialize");
        assert!(
            encoded.get("portAuthorities").is_some(),
            "remote delta must preserve the typed port authority field"
        );
    }

    #[test]
    fn listener_generation_lease_rejects_overlap_and_releases_exact_owner() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let generation = service
            .inner
            .native_runtime_generation
            .load(Ordering::Acquire);
        let first = super::acquire_listener_lease(&service.inner, 43871, generation)
            .expect("first listener lease should install");
        assert!(first.is_current());
        assert!(super::acquire_listener_lease(&service.inner, 43871, generation).is_err());
        drop(first);
        let second = super::acquire_listener_lease(&service.inner, 43871, generation)
            .expect("released listener lease should be reusable");
        service
            .inner
            .native_runtime_generation
            .fetch_add(1, Ordering::SeqCst);
        assert!(!second.is_current(), "stale generation must lose its lease");
    }

    #[test]
    fn external_listener_wins_typed_bind_conflict_without_being_harmed() {
        let _profile = TestProfileGuard::new("external-listener-bind-conflict");
        let occupied = TcpListener::bind(("127.0.0.1", 0)).expect("external listener should bind");
        let port = occupied
            .local_addr()
            .expect("external listener address")
            .port();
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (bind_failed_tx, bind_failed_rx) = mpsc::sync_channel(1);
        *service
            .inner
            .native_lifecycle_test_hook
            .write()
            .expect("native lifecycle hook lock") = Some(Arc::new(move |event| {
            if event == super::NativeLifecycleTestEvent::ListenerBindFailed {
                let _ = bind_failed_tx.send(());
            }
        }));
        service
            .update_native_listener_settings(true, "127.0.0.1".to_string(), port)
            .expect("native listener settings should persist");
        bind_failed_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("external bind conflict should be published without polling");
        assert!(service
            .status()
            .listener_error
            .as_deref()
            .is_some_and(|error| error.contains("external bind conflict")));
        assert!(
            TcpStream::connect(("127.0.0.1", port)).is_ok(),
            "external listener stopped accepting after DevManager bind conflict"
        );
        drop(service);
        drop(occupied);
    }

    #[test]
    fn external_web_listener_wins_typed_bind_conflict_without_being_harmed() {
        let _profile = TestProfileGuard::new("external-web-listener-bind-conflict");
        let occupied = TcpListener::bind(("127.0.0.1", 0)).expect("external listener should bind");
        let port = occupied
            .local_addr()
            .expect("external listener address")
            .port();
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (bind_failed_tx, bind_failed_rx) = mpsc::sync_channel(1);
        *service
            .inner
            .native_lifecycle_test_hook
            .write()
            .expect("native lifecycle hook lock") = Some(Arc::new(move |event| {
            if event == super::NativeLifecycleTestEvent::WebListenerBindFailed {
                let _ = bind_failed_tx.send(());
            }
        }));
        service
            .update_web_listener_settings(true, "127.0.0.1".to_string(), port)
            .expect("web listener settings should persist");
        bind_failed_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("external web bind conflict should be published without polling");
        assert!(service
            .status()
            .web_listener_error
            .as_deref()
            .is_some_and(|error| error.contains("external bind conflict")));
        assert!(
            TcpStream::connect(("127.0.0.1", port)).is_ok(),
            "external web listener stopped accepting after DevManager bind conflict"
        );
        drop(service);
        drop(occupied);
    }

    #[test]
    fn current_snapshot_only_includes_open_tab_sessions() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        if let Ok(mut shared_state) = service.inner.shared_state.write() {
            shared_state.open_tabs = vec![
                SessionTab {
                    id: "server-tab".to_string(),
                    tab_type: TabType::Server,
                    project_id: "project-1".to_string(),
                    command_id: Some("server-session".to_string()),
                    ..SessionTab::default()
                },
                SessionTab {
                    id: "claude-tab".to_string(),
                    tab_type: TabType::Claude,
                    project_id: "project-1".to_string(),
                    pty_session_id: Some("ai-session".to_string()),
                    provider_session_id: None,
                    ..SessionTab::default()
                },
            ];
        }
        service.set_session_bootstrap_provider(Some(Arc::new(|session_id| {
            Some(RemoteSessionBootstrap {
                session_id: session_id.to_string(),
                runtime: session_view(session_id).runtime,
                screen: session_view(session_id).screen,
                replay_bytes: format!("{session_id}\r\n").into_bytes(),
            })
        })));

        let snapshot = current_snapshot(&service.inner, "client-1");

        assert!(snapshot.session_views.contains_key("server-session"));
        assert!(snapshot.session_views.contains_key("ai-session"));
        assert!(!snapshot.session_views.contains_key("stale-session"));
    }

    #[test]
    fn light_snapshot_does_not_call_session_bootstrap_provider() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        if let Ok(mut shared_state) = service.inner.shared_state.write() {
            shared_state.open_tabs = vec![SessionTab {
                id: "server-tab".to_string(),
                tab_type: TabType::Server,
                project_id: "project-1".to_string(),
                command_id: Some("server-session".to_string()),
                ..SessionTab::default()
            }];
        }
        let calls = Arc::new(AtomicU64::new(0));
        let provider_calls = calls.clone();
        service.set_session_bootstrap_provider(Some(Arc::new(move |session_id| {
            provider_calls.fetch_add(1, Ordering::Relaxed);
            Some(RemoteSessionBootstrap {
                session_id: session_id.to_string(),
                runtime: session_view(session_id).runtime,
                screen: session_view(session_id).screen,
                replay_bytes: format!("{session_id}\r\n").into_bytes(),
            })
        })));

        let snapshot = light_snapshot(&service.inner, "client-1");

        assert!(snapshot.session_views.is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(snapshot.app_state.open_tabs.len(), 1);
    }

    #[test]
    fn remote_clients_start_in_viewer_mode_until_they_take_control() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        assert!(!current_controller_allows(&service.inner, "client-1"));
        service.take_local_control();
        assert!(!current_controller_allows(&service.inner, "client-1"));
        if let Ok(mut controller) = service.inner.controller_client_id.write() {
            *controller = Some("client-1".to_string());
        }
        assert!(current_controller_allows(&service.inner, "client-1"));
        assert!(!current_controller_allows(&service.inner, "client-2"));
    }

    #[test]
    fn host_status_reports_last_connection_note() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        set_last_connection_note(
            &service.inner,
            "Client disconnected before handshake.".to_string(),
            true,
        );

        let status = service.status();
        assert_eq!(
            status.last_connection_note.as_deref(),
            Some("Client disconnected before handshake.")
        );
        assert!(status.last_connection_is_error);
    }

    #[test]
    fn host_status_reports_latency_stats() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        service.record_input_write_latency(now_epoch_ms().saturating_sub(5));

        let latency = service.status().latency;
        assert!(latency.input_enqueue_to_host_write_ms.is_some());
    }

    #[test]
    fn handshake_stage_error_explains_early_host_disconnects() {
        let message = format_handshake_stage_error(
            "127.0.0.1",
            43871,
            "write",
            "Write failed: connection aborted",
        );

        assert!(message.contains("Handshake failed: Write failed: connection aborted"));
        assert!(message.contains("127.0.0.1:43871"));
        assert!(message.contains("host-side error"));
        assert!(message.contains("same remote build"));
    }

    #[test]
    fn loopback_host_and_client_complete_remote_handshake() {
        let _profile = TestProfileGuard::new("loopback-handshake");
        let port = reserve_free_tcp_port();
        let mut config = RemoteHostConfig {
            enabled: false,
            bind_address: "127.0.0.1".to_string(),
            port,
            ..RemoteHostConfig::default()
        };
        let pair_token = config.pairing_token.clone();
        let expected_server_id = config.server_id.clone();
        let service = RemoteHostService::new(config.clone());
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel();
        *service
            .inner
            .native_lifecycle_test_hook
            .write()
            .expect("native lifecycle hook lock") = Some(Arc::new(move |event| {
            lifecycle_tx
                .send(event)
                .expect("native lifecycle observer should remain");
        }));
        config.enabled = true;
        service.apply_config(config.clone());
        assert_eq!(
            lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("remote host never started listening"),
            super::NativeLifecycleTestEvent::ListenerStarted
        );

        let result = RemoteClientHandle::connect(
            "127.0.0.1",
            port,
            "Test Client",
            ClientAuth::PairToken { token: pair_token },
            None,
        )
        .expect("loopback remote connect should succeed");

        assert_eq!(result.server_id, expected_server_id);
        assert!(!result.client_id.trim().is_empty());
        assert!(!result.client_token.trim().is_empty());
        assert!(!result.certificate_fingerprint.trim().is_empty());
        assert!(!result.you_have_control);
        assert_eq!(result.snapshot.server_id, expected_server_id);

        assert_eq!(
            lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("host never registered connected client"),
            super::NativeLifecycleTestEvent::ClientRegistered
        );

        result.client.disconnect();

        assert_eq!(
            lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("host never observed client disconnect"),
            super::NativeLifecycleTestEvent::ClientRemoved
        );

        config.enabled = false;
        service.apply_config(config);
    }

    #[test]
    fn native_input_callback_can_replace_its_own_handler_without_deadlock() {
        let _profile = TestProfileGuard::new("native-input-handler-reentry");
        let port = reserve_free_tcp_port();
        let mut config = RemoteHostConfig {
            enabled: false,
            bind_address: "127.0.0.1".to_string(),
            port,
            ..RemoteHostConfig::default()
        };
        let pair_token = config.pairing_token.clone();
        let service = RemoteHostService::new(config.clone());
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel();
        *service
            .inner
            .native_lifecycle_test_hook
            .write()
            .expect("native lifecycle hook lock") = Some(Arc::new(move |event| {
            lifecycle_tx
                .send(event)
                .expect("native lifecycle observer should remain");
        }));

        let (callback_done_tx, callback_done_rx) = mpsc::sync_channel(1);
        let reentrant_service = service.clone();
        service.set_terminal_input_handler(Some(Arc::new(move |_, _| {
            reentrant_service.set_terminal_input_handler(None);
            callback_done_tx
                .send(())
                .expect("callback observer should remain");
            Ok(())
        })));

        config.enabled = true;
        service.apply_config(config.clone());
        assert_eq!(
            lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("remote host never started listening"),
            super::NativeLifecycleTestEvent::ListenerStarted
        );
        let result = RemoteClientHandle::connect(
            "127.0.0.1",
            port,
            "Reentrant callback client",
            ClientAuth::PairToken { token: pair_token },
            None,
        )
        .expect("loopback remote connect should succeed");
        assert_eq!(
            lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("host never registered connected client"),
            super::NativeLifecycleTestEvent::ClientRegistered
        );

        result.client.take_control();
        result
            .client
            .send_terminal_input(RemoteTerminalInput::Text {
                session_id: "alpha".to_string(),
                text: "hello".to_string(),
            });
        callback_done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("native callback deadlocked while replacing its own handler");

        result.client.disconnect();
        // Join/retain the exact reader owner; sending Disconnect alone does not
        // close its socket. ClientRemoved follows durable last_seen persistence,
        // not the callback whose short deadlock budget was checked above.
        drop(result);
        assert_eq!(
            lifecycle_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("host never observed client disconnect"),
            super::NativeLifecycleTestEvent::ClientRemoved
        );
        config.enabled = false;
        service.apply_config(config);
    }

    #[test]
    fn dropping_last_remote_client_handle_joins_its_reader_connection() {
        let _profile = TestProfileGuard::new("remote-client-reader-drop");
        let port = reserve_free_tcp_port();
        let mut config = RemoteHostConfig {
            enabled: false,
            bind_address: "127.0.0.1".to_string(),
            port,
            ..RemoteHostConfig::default()
        };
        let pair_token = config.pairing_token.clone();
        let service = RemoteHostService::new(config.clone());
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel();
        *service
            .inner
            .native_lifecycle_test_hook
            .write()
            .expect("native lifecycle hook lock") = Some(Arc::new(move |event| {
            lifecycle_tx
                .send(event)
                .expect("native lifecycle observer should remain");
        }));
        config.enabled = true;
        service.apply_config(config);
        assert_eq!(
            lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("remote host never started listening"),
            super::NativeLifecycleTestEvent::ListenerStarted
        );
        let result = RemoteClientHandle::connect(
            "127.0.0.1",
            port,
            "Drop client",
            ClientAuth::PairToken { token: pair_token },
            None,
        )
        .expect("loopback remote connect should succeed");
        assert_eq!(
            lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("host never registered connected client"),
            super::NativeLifecycleTestEvent::ClientRegistered
        );
        let (owner_event_tx, owner_event_rx) = mpsc::channel();
        let reader_event_tx = owner_event_tx.clone();
        *result
            .client
            .inner
            .reader_exit_test_hook
            .write()
            .expect("reader exit hook lock") = Some(Arc::new(move || {
            reader_event_tx
                .send("reader-exited")
                .expect("reader observer should remain");
        }));
        let client_inner = Arc::downgrade(&result.client.inner);

        let drop_thread = thread::spawn(move || {
            drop(result);
            owner_event_tx
                .send("owner-dropped")
                .expect("owner observer should remain");
        });

        assert_eq!(
            owner_event_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("client reader should exit before its owner returns"),
            "reader-exited",
            "last client owner returned before joining its reader"
        );
        assert_eq!(
            owner_event_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("last client owner should finish after joining its reader"),
            "owner-dropped"
        );
        drop_thread.join().expect("client owner drop should finish");
        assert_eq!(
            lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("host should observe last client owner disconnect"),
            super::NativeLifecycleTestEvent::ClientRemoved
        );
        assert!(
            client_inner.upgrade().is_none(),
            "client reader retained its state after connection teardown"
        );
        drop(service);
    }

    #[test]
    fn native_client_receives_output_while_bootstrap_lookup_blocks() {
        let _profile = TestProfileGuard::new("native-bootstrap-output");
        let port = reserve_free_tcp_port();
        let mut config = RemoteHostConfig {
            enabled: true,
            bind_address: "127.0.0.1".to_string(),
            port,
            ..RemoteHostConfig::default()
        };
        let pair_token = config.pairing_token.clone();
        let service = RemoteHostService::new(config.clone());
        let mut state = AppState::default();
        state.open_tabs = vec![SessionTab {
            id: "alpha-tab".to_string(),
            tab_type: TabType::Server,
            project_id: "project-1".to_string(),
            command_id: Some("alpha".to_string()),
            ..SessionTab::default()
        }];
        let mut runtime = RuntimeState::default();
        runtime
            .sessions
            .insert("alpha".to_string(), session_view("alpha").runtime);
        service.update_snapshot(state, runtime, HashMap::new());

        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let provider_release = release.clone();
        service.set_session_bootstrap_provider(Some(Arc::new(move |_session_id| {
            let (lock, cvar) = &*provider_release;
            let mut released = lock.lock().expect("gate lock");
            while !*released {
                let (next_released, wait_result) = cvar
                    .wait_timeout(released, Duration::from_secs(5))
                    .expect("gate wait");
                released = next_released;
                if wait_result.timed_out() {
                    break;
                }
            }
            None
        })));

        wait_for(
            || service.status().listening,
            Duration::from_secs(3),
            "remote host never started listening",
        );

        let result = RemoteClientHandle::connect(
            "127.0.0.1",
            port,
            "Test Client",
            ClientAuth::PairToken { token: pair_token },
            None,
        )
        .expect("loopback remote connect should succeed");

        wait_for(
            || service.subscribed_session_ids().contains("alpha"),
            Duration::from_secs(3),
            "native client never subscribed to the open terminal",
        );

        // Let the connection worker enter its readiness wait so this proves
        // channel-delivered output wakes promptly even when the peer sends no
        // further socket input.
        thread::sleep(Duration::from_millis(100));
        service.push_session_output("alpha", b"hello\r\n".to_vec());
        wait_for(
            || {
                result
                    .client
                    .session_screen_text("alpha")
                    .is_some_and(|text| text.contains("hello"))
            },
            Duration::from_millis(750),
            "native client did not paint output while bootstrap was blocked",
        );

        let (lock, cvar) = &*release;
        *lock.lock().expect("gate lock") = true;
        cvar.notify_all();
        result.client.disconnect();
        config.enabled = false;
        service.apply_config(config);
    }

    #[test]
    fn native_handshake_waits_for_durable_activity_before_success() {
        let _profile = TestProfileGuard::new("native-activity-durable-before-success");
        let config = RemoteHostConfig::default();
        let pair_token = config.pairing_token.clone();
        let service = RemoteHostService::new(config);

        let (persistence_entered_tx, persistence_entered_rx) = mpsc::sync_channel(1);
        let (persistence_release_tx, persistence_release_rx) = mpsc::sync_channel(0);
        let (persistence_settled_tx, persistence_settled_rx) = mpsc::sync_channel(1);
        let persistence_release_rx = Arc::new(Mutex::new(persistence_release_rx));
        let activity_write_seen = Arc::new(AtomicBool::new(false));
        let _persistence_hook = HostConfigPersistenceHookGuard::install(Arc::new({
            let persistence_release_rx = persistence_release_rx.clone();
            let activity_write_seen = activity_write_seen.clone();
            move |snapshot, phase| {
                if phase == HostConfigPersistenceTestPhase::BeforeWrite
                    && !snapshot.web.activity_log.is_empty()
                    && !activity_write_seen.swap(true, Ordering::SeqCst)
                {
                    persistence_entered_tx.send(()).map_err(|_| {
                        std::io::Error::new(
                            ErrorKind::BrokenPipe,
                            "activity persistence observer disappeared",
                        )
                    })?;
                    persistence_release_rx
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .recv_timeout(Duration::from_secs(3))
                        .map_err(|_| {
                            std::io::Error::new(
                                ErrorKind::TimedOut,
                                "activity persistence was not released",
                            )
                        })?;
                }
                if phase == HostConfigPersistenceTestPhase::AfterWrite
                    && !snapshot.web.activity_log.is_empty()
                {
                    let _ = persistence_settled_tx.try_send(());
                }
                Ok(())
            }
        }));

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("test listener should bind");
        let port = listener.local_addr().expect("test listener address").port();
        let native_runtime_generation = service
            .inner
            .native_runtime_generation
            .load(Ordering::SeqCst);
        let host_inner = service.inner.clone();
        let host_thread = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("test host should accept");
            handle_client_connection(host_inner, 1, stream, native_runtime_generation);
        });

        let (hello_tx, hello_rx) = mpsc::sync_channel(1);
        let (client_release_tx, client_release_rx) = mpsc::sync_channel(0);
        let client_thread = thread::spawn(move || {
            let mut stream = super::transport::connect_tls("127.0.0.1", port, None)
                .expect("test client should establish TLS")
                .stream;
            write_message(
                &mut stream,
                &ClientMessage::Hello {
                    protocol_version: super::PROTOCOL_VERSION,
                    client_label: "Studio MacBook".to_string(),
                    auth: ClientAuth::PairToken { token: pair_token },
                },
            )
            .expect("test client should write hello");
            let reply = read_message::<ServerMessage, _>(&mut stream);
            hello_tx.send(reply).expect("hello observer should remain");
            let _ = client_release_rx.recv_timeout(Duration::from_secs(3));
            let _ = write_message(&mut stream, &ClientMessage::Disconnect);
        });

        persistence_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("activity persistence should start");
        match hello_rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                panic!("handshake reply worker disappeared")
            }
            Ok(_) => panic!("host acknowledged HelloOk before its activity write was durable"),
        }
        persistence_release_tx
            .send(())
            .expect("activity persistence should still be waiting");
        persistence_settled_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("isolated activity file should finish its durable write");
        let reply = hello_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("host should settle the handshake after persistence")
            .expect("host should return a typed handshake reply");

        assert!(matches!(reply, ServerMessage::HelloOk { .. }));
        let on_disk = load_remote_machine_state().expect("durable remote state should load");
        assert!(on_disk.host.web.activity_log.iter().any(|event| {
            event.source == RemoteAccessSource::NativeApp
                && event.event_kind == RemoteAccessActivityKind::Connected
                && event.label == "Studio MacBook"
        }));

        drop(service);
        host_thread.join().expect("host connection should stop");
        let after_abrupt_stop =
            load_remote_machine_state().expect("remote state should survive abrupt host stop");
        assert!(after_abrupt_stop
            .host
            .web
            .activity_log
            .iter()
            .any(|event| event.label == "Studio MacBook"));
        let _ = client_release_tx.send(());
        client_thread.join().expect("test client should stop");
    }

    #[test]
    fn native_activity_persistence_failure_rejects_handshake_and_rolls_back_memory() {
        let _profile = TestProfileGuard::new("native-activity-persistence-error");
        let config = RemoteHostConfig::default();
        let pair_token = config.pairing_token.clone();
        let service = RemoteHostService::new(config);
        let _persistence_hook =
            HostConfigPersistenceHookGuard::install(Arc::new(|snapshot, phase| {
                if phase == HostConfigPersistenceTestPhase::AfterWrite
                    || snapshot.web.activity_log.is_empty()
                {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        ErrorKind::PermissionDenied,
                        "injected activity persistence failure",
                    ))
                }
            }));

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("test listener should bind");
        let port = listener.local_addr().expect("test listener address").port();
        let native_runtime_generation = service
            .inner
            .native_runtime_generation
            .load(Ordering::SeqCst);
        let host_inner = service.inner.clone();
        let host_thread = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("test host should accept");
            handle_client_connection(host_inner, 2, stream, native_runtime_generation);
        });

        let mut stream = super::transport::connect_tls("127.0.0.1", port, None)
            .expect("test client should establish TLS")
            .stream;
        write_message(
            &mut stream,
            &ClientMessage::Hello {
                protocol_version: super::PROTOCOL_VERSION,
                client_label: "Failure client".to_string(),
                auth: ClientAuth::PairToken { token: pair_token },
            },
        )
        .expect("test client should write hello");
        let reply = read_message::<ServerMessage, _>(&mut stream)
            .expect("host should return a typed handshake reply");
        let _ = write_message(&mut stream, &ClientMessage::Disconnect);
        host_thread.join().expect("host connection should stop");

        match reply {
            ServerMessage::HelloErr { message } => assert!(
                message.contains("injected activity persistence failure"),
                "{message}"
            ),
            other => panic!("persistence failure unexpectedly acknowledged handshake: {other:?}"),
        }
        assert!(service.config().paired_clients.is_empty());
        assert!(service.config().web.activity_log.is_empty());
        let path = remote_state_path().expect("isolated remote state path");
        if path.exists() {
            let on_disk = load_remote_machine_state().expect("isolated remote state should load");
            assert!(on_disk.host.paired_clients.is_empty());
            assert!(on_disk.host.web.activity_log.is_empty());
        }
    }

    #[test]
    fn blocked_native_admission_persistence_cannot_block_root_drop_or_commit_after_stop() {
        let _profile = TestProfileGuard::new("native-admission-persistence-drop-fence");
        let port = reserve_free_tcp_port();
        let mut config = RemoteHostConfig {
            enabled: false,
            bind_address: "127.0.0.1".to_string(),
            port,
            ..RemoteHostConfig::default()
        };
        let pair_token = config.pairing_token.clone();
        let root = RemoteHostService::new(config.clone());
        let (listener_started_tx, listener_started_rx) = mpsc::sync_channel(1);
        *root
            .inner
            .native_lifecycle_test_hook
            .write()
            .expect("native lifecycle hook lock") = Some(Arc::new(move |event| {
            if event == super::NativeLifecycleTestEvent::ListenerStarted {
                let _ = listener_started_tx.try_send(());
            }
        }));
        config.enabled = true;
        root.apply_config(config);
        listener_started_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("native listener should start");
        save_remote_machine_state(&RemoteMachineState {
            host: root.config(),
            known_hosts: Vec::new(),
        })
        .expect("seed isolated native admission state");
        let memory_before = root.config();
        let durable_before = load_remote_machine_state()
            .expect("load isolated native admission state before the attempt");

        let (persistence_entered_tx, persistence_entered_rx) = mpsc::sync_channel(1);
        let (persistence_release_tx, persistence_release_rx) = mpsc::sync_channel(0);
        let persistence_release_rx = Arc::new(Mutex::new(persistence_release_rx));
        let candidate_seen = Arc::new(AtomicBool::new(false));
        let _persistence_hook = HostConfigPersistenceHookGuard::install(Arc::new({
            let persistence_release_rx = persistence_release_rx.clone();
            let candidate_seen = candidate_seen.clone();
            move |snapshot, phase| {
                if phase == HostConfigPersistenceTestPhase::AfterWrite
                    && !snapshot.pending_admission_attempts.is_empty()
                    && !candidate_seen.swap(true, Ordering::SeqCst)
                {
                    persistence_entered_tx.send(snapshot.clone()).map_err(|_| {
                        std::io::Error::new(
                            ErrorKind::BrokenPipe,
                            "native persistence observer disappeared",
                        )
                    })?;
                    persistence_release_rx
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .recv()
                        .map_err(|_| {
                            std::io::Error::new(
                                ErrorKind::BrokenPipe,
                                "native persistence release disappeared",
                            )
                        })?;
                }
                Ok(())
            }
        }));
        let (worker_reaped_tx, worker_reaped_rx) = mpsc::sync_channel(1);
        *root
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("worker reaped hook lock") = Some(worker_reaped_tx);
        let host = Arc::downgrade(&root.inner);

        let (client_done_tx, client_done_rx) = mpsc::sync_channel(1);
        let client = thread::spawn(move || {
            let result = RemoteClientHandle::connect(
                "127.0.0.1",
                port,
                "Blocked persistence client",
                ClientAuth::PairToken { token: pair_token },
                None,
            );
            client_done_tx
                .send(result)
                .expect("native client result observer should remain");
        });
        let durable_attempt = persistence_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("native admission should enter persistence");
        assert_eq!(durable_attempt.paired_clients, memory_before.paired_clients);
        assert_eq!(
            durable_attempt.web.activity_log,
            memory_before.web.activity_log
        );
        assert_eq!(durable_attempt.pending_admission_attempts.len(), 1);
        assert_eq!(
            durable_attempt.pending_admission_attempts[0].source,
            RemoteAccessSource::NativeApp
        );

        let (drop_done_tx, drop_done_rx) = mpsc::sync_channel(1);
        let dropper = thread::spawn(move || {
            drop(root);
            let _ = drop_done_tx.try_send(());
        });
        let drop_returned_while_persistence_blocked = drop_done_rx
            .recv_timeout(super::REMOTE_WORKER_SHUTDOWN_TIMEOUT + Duration::from_millis(500))
            .is_ok();

        persistence_release_tx
            .send(())
            .expect("native persistence should remain blocked until explicit release");
        if !drop_returned_while_persistence_blocked {
            drop_done_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("root drop should finish after releasing the stale writer");
        }
        let reaped = worker_reaped_rx.recv_timeout(Duration::from_secs(3)).ok();
        let client_result = client_done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("native client should settle after root stop");
        let admission_rejected = match client_result {
            Ok(result) => {
                result.client.disconnect();
                false
            }
            Err(_) => true,
        };
        client.join().expect("native client worker should join");
        dropper.join().expect("root drop worker should join");

        assert!(
            drop_returned_while_persistence_blocked,
            "blocked native admission persistence held lifecycle authority across root drop"
        );
        assert!(
            admission_rejected,
            "a native admission acknowledged success after its root generation stopped"
        );
        assert!(
            reaped.is_some_and(|event| event.name.starts_with("remote-native-")),
            "the blocked native connection worker was not retained and reaped"
        );
        let durable_after = load_remote_machine_state()
            .expect("load isolated native admission state after root stop");
        assert_eq!(
            durable_after.host, durable_before.host,
            "stale native admission persistence changed the isolated durable host config"
        );
        assert_eq!(
            durable_after.host, memory_before,
            "stale native admission changed the host config"
        );
        assert!(
            host.upgrade().is_none(),
            "the reaped native admission worker retained the stopped host runtime"
        );
    }

    #[test]
    fn rejected_native_admission_reports_typed_uncertainty_when_compensation_fails() {
        let _profile = TestProfileGuard::new("native-admission-compensation-failure");
        let config = RemoteHostConfig::default();
        let pair_token = config.pairing_token.clone();
        let service = RemoteHostService::new(config);
        save_remote_machine_state(&RemoteMachineState {
            host: service.config(),
            known_hosts: Vec::new(),
        })
        .expect("seed isolated native compensation state");
        let durable_before = load_remote_machine_state()
            .expect("load isolated native compensation state before the attempt");
        let memory_before = service.config();
        let revision_before = service.config_revision();

        let (candidate_entered_tx, candidate_entered_rx) = mpsc::sync_channel(1);
        let (candidate_release_tx, candidate_release_rx) = mpsc::sync_channel(0);
        let candidate_release_rx = Arc::new(Mutex::new(candidate_release_rx));
        let candidate_seen = Arc::new(AtomicBool::new(false));
        let compensation_failed = Arc::new(AtomicBool::new(false));
        let baseline_host = durable_before.host.clone();
        let _persistence_hook = HostConfigPersistenceHookGuard::install(Arc::new({
            let candidate_release_rx = candidate_release_rx.clone();
            let candidate_seen = candidate_seen.clone();
            let compensation_failed = compensation_failed.clone();
            move |snapshot, phase| {
                if phase != HostConfigPersistenceTestPhase::BeforeWrite {
                    return Ok(());
                }
                if snapshot != &baseline_host && !candidate_seen.swap(true, Ordering::SeqCst) {
                    candidate_entered_tx.send(()).map_err(|_| {
                        std::io::Error::new(
                            ErrorKind::BrokenPipe,
                            "native compensation observer disappeared",
                        )
                    })?;
                    candidate_release_rx
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .recv()
                        .map_err(|_| {
                            std::io::Error::new(
                                ErrorKind::BrokenPipe,
                                "native compensation release disappeared",
                            )
                        })?;
                } else if snapshot == &baseline_host
                    && candidate_seen.load(Ordering::SeqCst)
                    && !compensation_failed.swap(true, Ordering::SeqCst)
                {
                    return Err(std::io::Error::new(
                        ErrorKind::PermissionDenied,
                        "injected conditional compensation failure",
                    ));
                }
                Ok(())
            }
        }));

        let authentication = prepare_native_client_authentication(
            ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_label: "Uncertain compensation client".to_string(),
                auth: ClientAuth::PairToken { token: pair_token },
            },
            Some(Some("127.0.0.6".to_string())),
        )
        .expect("prepare native compensation authentication");
        let generation = service
            .inner
            .native_runtime_generation
            .load(Ordering::Acquire);
        let inner = service.inner.clone();
        let (sender, _receiver) = mpsc::channel();
        let admission = thread::spawn(move || {
            admit_native_client(
                &inner,
                generation,
                601,
                &authentication,
                test_connected_client("uncertain-native", sender, None),
            )
        });
        candidate_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("native candidate should reach durable persistence");
        {
            let _lifecycle = service.inner.lifecycle_lock.lock().expect("lifecycle lock");
            service
                .inner
                .native_runtime_generation
                .fetch_add(1, Ordering::SeqCst);
        }
        candidate_release_tx
            .send(())
            .expect("release native candidate persistence");

        let error = admission
            .join()
            .expect("native admission worker should join")
            .expect_err("failed compensation must reject admission");
        assert!(matches!(
            error,
            HostConfigAdmissionError::DurabilityUncertain { .. }
        ));
        assert!(compensation_failed.load(Ordering::SeqCst));
        assert_eq!(service.config(), memory_before);
        assert_eq!(service.config_revision(), revision_before);
        assert!(service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .is_empty());
        let uncertain = load_remote_machine_state()
            .expect("load uncertain durable state")
            .host;
        assert_eq!(uncertain.paired_clients, durable_before.host.paired_clients);
        assert_eq!(
            uncertain.web.activity_log,
            durable_before.host.web.activity_log
        );
        assert_eq!(uncertain.pending_admission_attempts.len(), 1);
        assert_eq!(
            uncertain.pending_admission_attempts[0].source,
            RemoteAccessSource::NativeApp
        );
    }

    #[test]
    fn native_disconnect_durably_persists_last_seen_before_worker_join() {
        let _profile = TestProfileGuard::new("native-disconnect-durable-last-seen");
        let port = reserve_free_tcp_port();
        let mut config = RemoteHostConfig {
            enabled: false,
            bind_address: "127.0.0.1".to_string(),
            port,
            ..RemoteHostConfig::default()
        };
        let pair_token = config.pairing_token.clone();
        let root = RemoteHostService::new(config.clone());
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel();
        let (persistence_order_tx, persistence_order_rx) = mpsc::channel();
        let client_removed_order_tx = persistence_order_tx.clone();
        *root
            .inner
            .native_lifecycle_test_hook
            .write()
            .expect("native lifecycle hook lock") = Some(Arc::new(move |event| {
            if event == super::NativeLifecycleTestEvent::ClientRemoved {
                client_removed_order_tx
                    .send("client-removed")
                    .expect("disconnect order observer should remain");
            }
            lifecycle_tx
                .send(event)
                .expect("native lifecycle observer should remain");
        }));
        config.enabled = true;
        root.apply_config(config);
        assert_eq!(
            lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("native listener should start"),
            super::NativeLifecycleTestEvent::ListenerStarted
        );

        let connected = RemoteClientHandle::connect(
            "127.0.0.1",
            port,
            "Durable disconnect client",
            ClientAuth::PairToken { token: pair_token },
            None,
        )
        .expect("native client should connect");
        assert_eq!(
            lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("host should register native client"),
            super::NativeLifecycleTestEvent::ClientRegistered
        );
        let client_id = connected.client_id.clone();
        super::mutate_host_config(&root.inner, |config| {
            config
                .paired_clients
                .iter_mut()
                .find(|client| client.client_id == client_id)
                .expect("paired client should remain present")
                .last_seen_epoch_ms = None;
        })
        .expect("test should persist the cleared last-seen value");
        assert!(
            load_remote_machine_state()
                .expect("persisted remote state")
                .host
                .paired_clients
                .iter()
                .find(|client| client.client_id == connected.client_id)
                .expect("persisted paired client")
                .last_seen_epoch_ms
                .is_none(),
            "test precondition should be durable"
        );

        let (persistence_entered_tx, persistence_entered_rx) = mpsc::sync_channel(1);
        let (persistence_release_tx, persistence_release_rx) = mpsc::sync_channel(0);
        let persistence_release_rx = Arc::new(Mutex::new(persistence_release_rx));
        let persistence_client_id = connected.client_id.clone();
        let persistence_order_tx = persistence_order_tx.clone();
        let persistence_entered = Arc::new(AtomicBool::new(false));
        let _persistence_hook = HostConfigPersistenceHookGuard::install(Arc::new({
            let persistence_release_rx = persistence_release_rx.clone();
            let persistence_entered = persistence_entered.clone();
            move |snapshot, phase| {
                let has_last_seen = snapshot.paired_clients.iter().any(|client| {
                    client.client_id == persistence_client_id && client.last_seen_epoch_ms.is_some()
                });
                if phase == HostConfigPersistenceTestPhase::BeforeWrite
                    && has_last_seen
                    && !persistence_entered.swap(true, Ordering::SeqCst)
                {
                    persistence_entered_tx.send(()).map_err(|_| {
                        std::io::Error::new(
                            ErrorKind::BrokenPipe,
                            "disconnect persistence observer disappeared",
                        )
                    })?;
                    persistence_release_rx
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .recv()
                        .map_err(|_| {
                            std::io::Error::new(
                                ErrorKind::BrokenPipe,
                                "disconnect persistence was not released",
                            )
                        })?;
                }
                if phase == HostConfigPersistenceTestPhase::AfterWrite && has_last_seen {
                    persistence_order_tx
                        .send("last-seen-persisted")
                        .map_err(|_| {
                            std::io::Error::new(
                                ErrorKind::BrokenPipe,
                                "disconnect order observer disappeared",
                            )
                        })?;
                }
                Ok(())
            }
        }));

        connected.client.disconnect();
        persistence_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("disconnect should enter synchronous last-seen persistence");
        persistence_release_tx
            .send(())
            .expect("disconnect persistence should still be waiting");
        assert_eq!(
            persistence_order_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("disconnect should finish the last-seen write"),
            "last-seen-persisted"
        );
        assert_eq!(
            lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("host should observe native disconnect"),
            super::NativeLifecycleTestEvent::ClientRemoved
        );
        assert_eq!(
            persistence_order_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("disconnect should report removal after the durable write"),
            "client-removed"
        );
        drop(connected);
        drop(root);

        let persisted = load_remote_machine_state().expect("reload durable disconnect state");
        assert!(
            persisted
                .host
                .paired_clients
                .iter()
                .find(|client| client.client_id == client_id)
                .expect("persisted paired client should remain")
                .last_seen_epoch_ms
                .is_some(),
            "joined native disconnect updated last-seen only in memory"
        );
    }

    #[test]
    fn take_control_updates_client_snapshot_immediately() {
        let handle = sample_remote_client_handle("client-1");

        handle.take_control();

        let snapshot = handle.latest_snapshot().expect("snapshot should exist");
        assert!(snapshot.you_have_control);
        assert_eq!(snapshot.controller_client_id.as_deref(), Some("client-1"));
    }

    #[test]
    fn release_control_updates_client_snapshot_immediately() {
        let handle = sample_remote_client_handle("client-1");
        handle.take_control();

        handle.release_control();

        let snapshot = handle.latest_snapshot().expect("snapshot should exist");
        assert!(!snapshot.you_have_control);
        assert!(snapshot.controller_client_id.is_none());
    }

    #[test]
    fn client_latency_stats_track_output_and_paint() {
        let handle = sample_remote_client_handle("client-1");
        handle.note_output_received(now_epoch_ms().saturating_sub(3));
        handle.note_terminal_paint_ready();

        let latency = handle.latency_stats();
        assert!(latency.output_host_to_client_ms.is_some());
        assert!(latency.output_client_to_paint_ms.is_some());
    }

    #[test]
    fn remote_client_applies_output_before_bootstrap_when_runtime_is_known() {
        let handle = sample_remote_client_handle("client-1");
        let mut snapshot = handle.latest_snapshot().expect("snapshot should exist");
        snapshot
            .runtime_state
            .sessions
            .insert("alpha".to_string(), session_view("alpha").runtime);
        if let Ok(mut latest) = handle.inner.latest_snapshot.write() {
            *latest = Some(snapshot);
        }

        assert!(apply_remote_session_output(
            &handle.inner,
            "alpha",
            b"hello\r\n",
        ));

        let text = handle
            .session_screen_text("alpha")
            .expect("replica should be created from runtime snapshot");
        assert!(text.contains("hello"));
    }

    #[test]
    fn forward_cancellation_interrupts_a_stalled_write_and_closes_both_endpoints() {
        fn connected_pair() -> (TcpStream, TcpStream) {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("pair listener");
            let address = listener.local_addr().expect("pair address");
            let peer = TcpStream::connect(address).expect("pair client");
            let (worker, _) = listener.accept().expect("pair accept");
            (peer, worker)
        }

        let (mut left_peer, mut left_worker) = connected_pair();
        let (mut right_peer, mut right_worker) = connected_pair();
        let cancellation = Arc::new(ForwardCancellation::default());
        assert!(cancellation.register(&left_worker));
        assert!(cancellation.register(&right_worker));
        let (write_blocked_tx, write_blocked_rx) = mpsc::sync_channel(1);
        cancellation.set_write_blocked_observer(Some(write_blocked_tx));

        let copy_cancellation = cancellation.clone();
        let (copy_done_tx, copy_done_rx) = mpsc::sync_channel(1);
        let copy = thread::spawn(move || {
            let result = copy_bidirectional(
                &mut left_worker,
                &mut right_worker,
                &copy_cancellation,
                || false,
            );
            copy_done_tx
                .send(result)
                .expect("copy completion observer should remain");
        });
        let (writer_done_tx, writer_done_rx) = mpsc::sync_channel(1);
        let writer = thread::spawn(move || {
            let chunk = [0x5a_u8; 64 * 1024];
            let result = loop {
                if let Err(error) = left_peer.write_all(&chunk) {
                    break error;
                }
            };
            writer_done_tx
                .send(result)
                .expect("writer completion observer should remain");
        });

        write_blocked_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("forward should reach a genuinely stalled nonblocking write");
        let cancellation_started = Instant::now();
        cancellation.cancel();
        let copy_result = copy_done_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("stalled forward should settle within the lifecycle deadline");
        assert!(
            cancellation_started.elapsed() < Duration::from_millis(500),
            "stalled forward exceeded the bounded cancellation deadline"
        );
        assert!(
            copy_result.is_ok(),
            "cancelled forward failed: {copy_result:?}"
        );
        writer_done_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("cancelling the left endpoint should wake its blocked peer writer");
        copy.join().expect("copy worker should join");
        writer.join().expect("peer writer should join");

        right_peer
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("right peer read timeout");
        let mut drained = [0_u8; 64 * 1024];
        loop {
            match right_peer.read(&mut drained) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) => panic!("right endpoint did not close after cancellation: {error}"),
            }
        }
    }

    #[test]
    fn revoked_native_client_cannot_forward_after_tls_accept_before_hello() {
        let _profile = TestProfileGuard::new("revoke-native-port-forward-before-hello");
        let host_port = reserve_free_tcp_port();
        let upstream = TcpListener::bind(("127.0.0.1", 0)).expect("upstream server should bind");
        let server_port = upstream
            .local_addr()
            .expect("upstream address should be available")
            .port();
        let mut config = RemoteHostConfig {
            enabled: true,
            bind_address: "127.0.0.1".to_string(),
            port: host_port,
            ..RemoteHostConfig::default()
        };
        let server_id = config.server_id.clone();
        config.paired_clients.push(PairedRemoteClient {
            client_id: "revoked-client".to_string(),
            label: "Revoked laptop".to_string(),
            auth_token: "revoked-secret".to_string(),
            last_seen_epoch_ms: Some(1),
        });
        save_remote_machine_state(&RemoteMachineState {
            host: config.clone(),
            known_hosts: Vec::new(),
        })
        .expect("seed remote state");
        let service = RemoteHostService::new(config);
        service.update_snapshot(
            managed_server_state(server_port),
            managed_server_runtime("command-web", 4242, server_port),
            HashMap::from([(
                server_port,
                PortStatus {
                    port: server_port,
                    in_use: true,
                    pid: Some(4242),
                    process_name: Some("node".to_string()),
                },
            )]),
        );

        wait_for(
            || service.status().listening,
            Duration::from_secs(3),
            "remote host never started listening",
        );
        let mut stream = super::transport::connect_tls("127.0.0.1", host_port, None)
            .expect("TLS-only native client should complete transport handshake")
            .stream;

        assert!(service.revoke_paired_client("revoked-client"));
        write_message(
            &mut stream,
            &ClientMessage::PortForwardHello {
                protocol_version: super::PROTOCOL_VERSION,
                server_id,
                client_id: "revoked-client".to_string(),
                auth_token: "revoked-secret".to_string(),
                requested_port: server_port,
            },
        )
        .expect("withheld port-forward hello should write");

        match read_message::<ServerMessage, _>(&mut stream)
            .expect("host should answer the revoked port-forward hello")
        {
            ServerMessage::HelloErr { message } => {
                assert!(message.contains("no longer valid"), "{message}");
            }
            other => panic!("revoked client unexpectedly opened a port forward: {other:?}"),
        }
        drop(upstream);
    }

    #[test]
    fn revoking_native_client_stops_an_active_port_forward() {
        let _profile = TestProfileGuard::new("revoke-active-native-port-forward");
        let host_port = reserve_free_tcp_port();
        let upstream = TcpListener::bind(("127.0.0.1", 0)).expect("upstream server should bind");
        let server_port = upstream
            .local_addr()
            .expect("upstream address should be available")
            .port();

        let mut config = RemoteHostConfig {
            enabled: true,
            bind_address: "127.0.0.1".to_string(),
            port: host_port,
            ..RemoteHostConfig::default()
        };
        let server_id = config.server_id.clone();
        config.paired_clients.push(PairedRemoteClient {
            client_id: "active-client".to_string(),
            label: "Active laptop".to_string(),
            auth_token: "active-secret".to_string(),
            last_seen_epoch_ms: Some(1),
        });
        save_remote_machine_state(&RemoteMachineState {
            host: config.clone(),
            known_hosts: Vec::new(),
        })
        .expect("seed remote state");
        let service = RemoteHostService::new(config);
        service.update_snapshot(
            managed_server_state(server_port),
            managed_server_runtime("command-web", 4242, server_port),
            HashMap::from([(
                server_port,
                PortStatus {
                    port: server_port,
                    in_use: true,
                    pid: Some(4242),
                    process_name: Some("node".to_string()),
                },
            )]),
        );

        wait_for(
            || service.status().listening,
            Duration::from_secs(3),
            "remote host never started listening",
        );
        let mut stream = super::transport::connect_tls("127.0.0.1", host_port, None)
            .expect("native port-forward TLS should connect")
            .stream;
        write_message(
            &mut stream,
            &ClientMessage::PortForwardHello {
                protocol_version: super::PROTOCOL_VERSION,
                server_id,
                client_id: "active-client".to_string(),
                auth_token: "active-secret".to_string(),
                requested_port: server_port,
            },
        )
        .expect("port-forward hello should write");
        match read_message::<ServerMessage, _>(&mut stream).expect("host should answer hello") {
            ServerMessage::HelloErr { message } => {
                assert!(
                    message.contains("not a live DevManager server port"),
                    "{message}"
                );
            }
            other => panic!("legacy PID-only status unexpectedly opened a port forward: {other:?}"),
        }
        drop(upstream);
    }

    #[test]
    fn port_forward_tunnels_bytes_to_a_live_managed_server_port() {
        let _profile = TestProfileGuard::new("live-port-forward");
        let host_port = reserve_free_tcp_port();
        let server_port = reserve_free_tcp_port();

        let mut config = RemoteHostConfig {
            enabled: true,
            bind_address: "127.0.0.1".to_string(),
            port: host_port,
            ..RemoteHostConfig::default()
        };
        let pair_token = config.pairing_token.clone();
        let service = RemoteHostService::new(config.clone());
        service.update_snapshot(
            managed_server_state(server_port),
            managed_server_runtime("command-web", 4242, server_port),
            HashMap::from([(
                server_port,
                PortStatus {
                    port: server_port,
                    in_use: true,
                    pid: Some(4242),
                    process_name: Some("node".to_string()),
                },
            )]),
        );

        wait_for(
            || service.status().listening,
            Duration::from_secs(3),
            "remote host never started listening",
        );

        let client = RemoteClientHandle::connect(
            "127.0.0.1",
            host_port,
            "Test Client",
            ClientAuth::PairToken { token: pair_token },
            None,
        )
        .expect("remote client should connect");

        let error = client
            .client
            .open_port_forward(server_port)
            .expect_err("legacy PID-only status must not authorize a port forward");
        assert!(
            error.contains("not a live DevManager server port"),
            "{error}"
        );

        client.client.disconnect();
        config.enabled = false;
        service.apply_config(config);
    }

    #[test]
    fn production_tls_forward_requires_and_uses_typed_port_authority() {
        let _profile = TestProfileGuard::new("production-typed-port-authority-forward");
        let host_port = reserve_free_tcp_port();
        let upstream = TcpListener::bind(("127.0.0.1", 0)).expect("upstream should bind");
        let server_port = upstream.local_addr().expect("upstream address").port();
        let mut config = RemoteHostConfig {
            enabled: false,
            bind_address: "127.0.0.1".to_string(),
            port: host_port,
            ..RemoteHostConfig::default()
        };
        let pair_token = config.pairing_token.clone();
        let service = RemoteHostService::new(config.clone());
        let (listener_started_tx, listener_started_rx) = mpsc::sync_channel(1);
        *service
            .inner
            .native_lifecycle_test_hook
            .write()
            .expect("native lifecycle hook lock") = Some(Arc::new(move |event| {
            if event == super::NativeLifecycleTestEvent::ListenerStarted {
                let _ = listener_started_tx.send(());
            }
        }));
        config.enabled = true;
        service.apply_config(config.clone());
        listener_started_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("production listener should start");

        let app = managed_server_state(server_port);
        let inventory = crate::services::ports_service::PortInventory::new();
        let live_snapshot = inventory
            .refresh(&[server_port])
            .expect("strict live port inventory should complete");
        assert!(
            live_snapshot.is_valid(),
            "strict live snapshot must validate"
        );
        assert!(
            live_snapshot.is_fresh_at(
                Instant::now(),
                crate::process::ports::DEFAULT_FREE_PROOF_MAX_AGE
            ),
            "strict live snapshot must be fresh"
        );
        let live_listener = live_snapshot
            .observation(server_port)
            .expect("live port observation")
            .listeners()
            .first()
            .expect("live listener identity");
        let process_id = live_listener.pid();
        let executable = live_listener
            .canonical_executable()
            .expect("strict live listener must include executable proof")
            .to_path_buf();
        let managed_identity = crate::process::identity::ManagedProcessIdentity::new(
            crate::process::identity::ManagedProcessId::new(
                process_id,
                live_listener.creation_time_100ns(),
            )
            .expect("live listener process identity should be valid"),
            executable,
        )
        .expect("live listener executable should canonicalize");
        let resource = ResourceFence::new(ResourceId::new(), 1);
        let managed = crate::process::ports::ManagedResourceSnapshot::new(
            crate::process::registry::ManagedProcessFence::new(
                resource,
                crate::process::identity::ProcessOwner::Host,
                managed_identity.clone(),
            ),
            crate::process::registry::ManagedProcessState::Running,
            vec![managed_identity],
            crate::process::ports::RegistryMembershipSnapshot::valid(
                1,
                1,
                Instant::now(),
                Duration::from_secs(5),
            ),
        );
        assert!(
            managed.is_fresh_at(Instant::now()),
            "managed live membership should be fresh: {:?}",
            managed.membership()
        );
        assert_eq!(
            live_snapshot
                .observation(server_port)
                .expect("live port observation")
                .listeners()[0]
                .pid(),
            process_id
        );
        assert_eq!(
            live_snapshot
                .observation(server_port)
                .expect("live port observation")
                .listeners()[0]
                .canonical_executable(),
            Some(managed.root().canonical_executable()),
            "strict listener executable must match the managed root"
        );
        assert!(
            managed.member_identities().iter().any(|member| {
                member.id().pid() == live_listener.pid()
                    && member.id().creation_time_100ns() == live_listener.creation_time_100ns()
                    && member.canonical_executable()
                        == live_listener.canonical_executable().unwrap()
            }),
            "strict listener identity must be a member of the managed live snapshot: listener={live_listener:?}, members={:?}",
            managed.member_identities()
        );
        assert!(
            live_snapshot.endpoints(server_port).is_empty()
                || live_snapshot
                    .endpoints(server_port)
                    .iter()
                    .all(|endpoint| { endpoint.identity() == *live_listener }),
            "strict endpoint identities must agree with the listener observation"
        );
        let classified = crate::process::ports::classify_port_authority_from_snapshot_at(
            &crate::process::ports::PortTarget::new(
                server_port,
                resource,
                crate::process::ports::ManagedPortHealth::Ready,
            ),
            &live_snapshot,
            Some(&managed),
            Instant::now(),
            Instant::now() + crate::process::ports::DEFAULT_FREE_PROOF_MAX_AGE,
        );
        assert_eq!(
            classified,
            crate::process::ports::PortAuthority::Managed,
            "strict live authority classification should prove ownership"
        );
        let observed_at = live_snapshot.observed_at();
        let live_status = crate::process::ports::project_port_status_from_snapshot_at(
            &crate::process::ports::PortTarget::new(
                server_port,
                resource,
                crate::process::ports::ManagedPortHealth::Ready,
            ),
            &live_snapshot,
            Some(&managed),
            Instant::now(),
            observed_at
                .checked_add(crate::process::ports::DEFAULT_FREE_PROOF_MAX_AGE)
                .expect("live snapshot deadline should fit"),
        );
        assert_eq!(
            live_status.kind(),
            crate::process::ports::PortStatusKind::ManagedHealthy,
            "forward authority must come from the strict live snapshot: {live_status:?}"
        );
        let capability = Arc::new(crate::process::ports::test_capability_from_snapshot(
            managed,
        ));
        let authority = RemotePortAuthority::from_rich(&live_status, now_epoch_ms())
            .with_snapshot_metadata(
                live_snapshot.publication_sequence(),
                capability.snapshot().membership_revision(),
                capability.snapshot().observation_sequence(),
            )
            .with_session_id("command-web")
            .with_managed_capability(capability.as_ref());
        let runtime = managed_server_runtime("command-web", process_id, server_port);
        let legacy_status = PortStatus {
            port: server_port,
            in_use: !live_status.listeners().is_empty(),
            pid: live_status.listener().map(|listener| listener.pid()),
            process_name: None,
        };
        service.update_snapshot_parts_with_authorities(
            Some(app),
            Some(runtime),
            Some(HashMap::from([(server_port, legacy_status)])),
            Some(HashMap::from([(server_port, authority)])),
        );
        service.update_managed_port_capabilities(HashMap::from([(server_port, capability)]));

        let (upstream_done_tx, upstream_done_rx) = mpsc::sync_channel(1);
        let upstream_thread = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().expect("forward should reach upstream");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("upstream read timeout");
            let mut request = [0_u8; 4];
            stream
                .read_exact(&mut request)
                .expect("upstream should receive request");
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").expect("upstream response");
            stream.flush().expect("upstream response flush");
            upstream_done_tx.send(()).expect("upstream observer");
        });

        let client = RemoteClientHandle::connect(
            "127.0.0.1",
            host_port,
            "Typed authority client",
            ClientAuth::PairToken { token: pair_token },
            None,
        )
        .expect("native client should connect through the real listener");
        let mut forward = client
            .client
            .open_port_forward(server_port)
            .expect("typed authority should authorize production TLS forward");
        forward.write_all(b"ping").expect("forward request");
        forward.flush().expect("forward request flush");
        let mut response = [0_u8; 4];
        forward.read_exact(&mut response).expect("forward response");
        assert_eq!(&response, b"pong");
        upstream_done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("upstream should finish production forward");
        drop(forward);
        client.client.disconnect();
        config.enabled = false;
        service.apply_config(config);
        upstream_thread.join().expect("upstream should stop");
    }

    #[test]
    fn dropping_root_service_stops_an_active_native_port_forward() {
        let _profile = TestProfileGuard::new("drop-native-port-forward");
        let upstream = TcpListener::bind(("127.0.0.1", 0)).expect("upstream should bind");
        let server_port = upstream.local_addr().expect("upstream address").port();
        let native_listener =
            TcpListener::bind(("127.0.0.1", 0)).expect("native test listener should bind");
        let host_port = native_listener
            .local_addr()
            .expect("native listener address")
            .port();

        let mut config = RemoteHostConfig::default();
        let server_id = config.server_id.clone();
        config.paired_clients.push(PairedRemoteClient {
            client_id: "forward-client".to_string(),
            label: "Forward client".to_string(),
            auth_token: "forward-secret".to_string(),
            last_seen_epoch_ms: Some(1),
        });
        let root = RemoteHostService::new(config);
        publish_live_managed_port(&root, server_port);

        let (upstream_closed_tx, upstream_closed_rx) = mpsc::sync_channel(1);
        let upstream_thread = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().expect("upstream should accept forward");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("upstream read timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .expect("upstream write timeout");
            let mut request = [0_u8; 4];
            stream
                .read_exact(&mut request)
                .expect("forward should deliver request bytes");
            assert_eq!(&request, b"ping");
            stream
                .write_all(b"pong")
                .expect("upstream should write response");
            stream.flush().expect("upstream response should flush");
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .expect("upstream close deadline");
            let closed = read_proves_socket_closed(&mut stream);
            upstream_closed_tx
                .send(closed)
                .expect("upstream close observer should remain");
        });

        let (forward_active_tx, forward_active_rx) = mpsc::sync_channel(1);
        let (client_closed_tx, client_closed_rx) = mpsc::sync_channel(1);
        let client_thread = thread::spawn(move || {
            let mut stream = super::transport::connect_tls("127.0.0.1", host_port, None)
                .expect("native port-forward TLS should connect")
                .stream;
            stream
                .sock
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("forward client read timeout");
            write_message(
                &mut stream,
                &ClientMessage::PortForwardHello {
                    protocol_version: super::PROTOCOL_VERSION,
                    server_id,
                    client_id: "forward-client".to_string(),
                    auth_token: "forward-secret".to_string(),
                    requested_port: server_port,
                },
            )
            .expect("port-forward hello should write");
            let handshake_deadline = Instant::now() + Duration::from_secs(3);
            let reply = read_message_until_cancelled::<ServerMessage, _, _>(&mut stream, || {
                Instant::now() >= handshake_deadline
            })
            .expect("host should answer port-forward hello");
            assert!(matches!(reply, ServerMessage::PortForwardOk));
            stream
                .write_all(b"ping")
                .expect("forward should accept bytes");
            stream.flush().expect("forward request should flush");
            let mut response = [0_u8; 4];
            stream
                .read_exact(&mut response)
                .expect("forward should return response bytes");
            forward_active_tx
                .send(response)
                .expect("forward observer should remain");
            stream
                .sock
                .set_read_timeout(Some(Duration::from_millis(500)))
                .expect("forward client close deadline");
            let closed = read_proves_socket_closed(&mut stream.sock);
            client_closed_tx
                .send(closed)
                .expect("client close observer should remain");
        });

        let (native_stream, _) = native_listener
            .accept()
            .expect("native test host should accept");
        let native_runtime_generation = root.inner.native_runtime_generation.load(Ordering::SeqCst);
        spawn_native_connection_worker(&root.inner, 1, native_stream, native_runtime_generation);
        assert_eq!(
            forward_active_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("real forward should exchange bytes"),
            *b"pong"
        );
        assert_eq!(
            root.inner
                .native_connection_workers
                .lock()
                .expect("native worker lock")
                .len(),
            1,
            "active forward must be owned by the host lifecycle"
        );
        let inner = Arc::downgrade(&root.inner);

        drop(root);

        assert!(upstream_closed_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("upstream should observe joined teardown"));
        assert!(client_closed_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("client should observe joined teardown"));
        upstream_thread.join().expect("upstream thread should stop");
        client_thread.join().expect("forward client should stop");
        assert!(
            inner.upgrade().is_none(),
            "joined active port forward retained the stopped host runtime"
        );
    }

    #[test]
    fn serial_native_lifecycle_settles_before_next_handshake() {
        let _profile = TestProfileGuard::new("serial-native-lifecycle");

        let first_port = reserve_free_tcp_port();
        let mut first_config = RemoteHostConfig {
            enabled: false,
            bind_address: "127.0.0.1".to_string(),
            port: first_port,
            ..RemoteHostConfig::default()
        };
        let first_pair_token = first_config.pairing_token.clone();
        let first_root = RemoteHostService::new(first_config.clone());
        let (first_lifecycle_tx, first_lifecycle_rx) = mpsc::channel();
        *first_root
            .inner
            .native_lifecycle_test_hook
            .write()
            .expect("first native lifecycle hook lock") = Some(Arc::new(move |event| {
            first_lifecycle_tx
                .send(event)
                .expect("first lifecycle observer should remain");
        }));
        first_config.enabled = true;
        first_root.apply_config(first_config);
        assert_eq!(
            first_lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("first remote host never started listening"),
            super::NativeLifecycleTestEvent::ListenerStarted
        );
        let first_client = RemoteClientHandle::connect(
            "127.0.0.1",
            first_port,
            "First client",
            ClientAuth::PairToken {
                token: first_pair_token,
            },
            None,
        )
        .expect("first remote client should connect");
        assert_eq!(
            first_lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("first remote host never registered its client"),
            super::NativeLifecycleTestEvent::ClientRegistered
        );
        let first_inner = Arc::downgrade(&first_root.inner);

        drop(first_root);

        assert!(
            first_inner.upgrade().is_none(),
            "first native connection worker survived bounded root teardown"
        );
        first_client.client.disconnect();

        let second_port = reserve_free_tcp_port();
        let mut second_config = RemoteHostConfig {
            enabled: false,
            bind_address: "127.0.0.1".to_string(),
            port: second_port,
            ..RemoteHostConfig::default()
        };
        let second_pair_token = second_config.pairing_token.clone();
        let second_root = RemoteHostService::new(second_config.clone());
        let (second_lifecycle_tx, second_lifecycle_rx) = mpsc::channel();
        *second_root
            .inner
            .native_lifecycle_test_hook
            .write()
            .expect("second native lifecycle hook lock") = Some(Arc::new(move |event| {
            second_lifecycle_tx
                .send(event)
                .expect("second lifecycle observer should remain");
        }));
        second_config.enabled = true;
        second_root.apply_config(second_config.clone());
        assert_eq!(
            second_lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("second remote host never started listening"),
            super::NativeLifecycleTestEvent::ListenerStarted
        );
        let second_client = RemoteClientHandle::connect(
            "127.0.0.1",
            second_port,
            "Second client",
            ClientAuth::PairToken {
                token: second_pair_token,
            },
            None,
        )
        .expect("second remote client should connect");
        assert_eq!(
            second_lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("second remote host never registered its client"),
            super::NativeLifecycleTestEvent::ClientRegistered
        );
        second_client.client.disconnect();
        assert_eq!(
            second_lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("second remote host never observed client disconnect"),
            super::NativeLifecycleTestEvent::ClientRemoved
        );
        second_config.enabled = false;
        second_root.apply_config(second_config);
    }

    #[test]
    fn native_restart_rejects_an_authenticated_client_paused_before_registration() {
        let _profile = TestProfileGuard::new("native-client-registration-fence");
        let port = reserve_free_tcp_port();
        let mut config = RemoteHostConfig {
            enabled: false,
            bind_address: "127.0.0.1".to_string(),
            port,
            ..RemoteHostConfig::default()
        };
        let pair_token = config.pairing_token.clone();
        let service = RemoteHostService::new(config.clone());
        let (listener_started_tx, listener_started_rx) = mpsc::sync_channel(1);
        *service
            .inner
            .native_lifecycle_test_hook
            .write()
            .expect("native lifecycle hook lock") = Some(Arc::new(move |event| {
            if event == super::NativeLifecycleTestEvent::ListenerStarted {
                listener_started_tx
                    .send(())
                    .expect("listener observer should remain");
            }
        }));
        config.enabled = true;
        service.apply_config(config.clone());
        listener_started_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("native listener should start");
        save_remote_machine_state(&RemoteMachineState {
            host: service.config(),
            known_hosts: Vec::new(),
        })
        .expect("seed durable native registration state");
        let durable_before = load_remote_machine_state()
            .expect("load durable native registration state before the attempt");
        let memory_before = service.config();

        let (registration_event_tx, registration_event_rx) = mpsc::sync_channel(3);
        let (registration_release_tx, registration_release_rx) = mpsc::sync_channel(0);
        let registration_release_rx = Arc::new(Mutex::new(registration_release_rx));
        *service
            .inner
            .client_registration_test_hook
            .write()
            .expect("client registration hook lock") = Some(Arc::new(move |event| {
            registration_event_tx
                .send(event)
                .expect("registration observer should remain");
            if event == ClientRegistrationTestEvent::BeforeFence {
                registration_release_rx
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .recv()
                    .expect("registration should be released");
            }
        }));

        let (client_done_tx, client_done_rx) = mpsc::sync_channel(1);
        let client = thread::spawn(move || {
            let result = RemoteClientHandle::connect(
                "127.0.0.1",
                port,
                "Registration fence client",
                ClientAuth::PairToken { token: pair_token },
                None,
            );
            client_done_tx
                .send(result)
                .expect("client result observer should remain");
        });
        assert_eq!(
            registration_event_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("native client should pause before registration"),
            ClientRegistrationTestEvent::BeforeFence
        );

        let (transition_tx, transition_rx) = mpsc::sync_channel(1);
        *service
            .inner
            .lifecycle_lock_acquired_test_hook
            .write()
            .expect("lifecycle transition hook lock") = Some(Arc::new(move || {
            transition_tx
                .send(())
                .expect("lifecycle transition observer should remain");
        }));
        let restart_service = service.clone();
        config.enabled = false;
        let (restart_done_tx, restart_done_rx) = mpsc::sync_channel(1);
        let restart = thread::spawn(move || {
            restart_service.apply_config(config);
            restart_done_tx
                .send(())
                .expect("restart observer should remain");
        });
        transition_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("restart should revoke the listener generation");
        registration_release_tx
            .send(())
            .expect("paused registration should still be waiting");

        let outcome = registration_event_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("registration should report its fenced outcome");
        let client_result = client_done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("client should settle after the generation is revoked");
        drop(client_result);
        client.join().expect("client worker should join");
        restart_done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("restart should finish after the client settles");
        restart.join().expect("restart worker should join");
        *service
            .inner
            .client_registration_test_hook
            .write()
            .expect("client registration hook lock") = None;

        assert_eq!(
            outcome,
            ClientRegistrationTestEvent::Rejected,
            "a revoked native generation admitted a late authenticated client"
        );
        assert!(
            service
                .inner
                .clients
                .lock()
                .expect("clients lock")
                .is_empty(),
            "a revoked native generation left a late client registered"
        );
        let memory_after = service.config();
        assert_eq!(
            memory_after.paired_clients, memory_before.paired_clients,
            "a rejected native admission changed paired credentials in memory"
        );
        assert_eq!(
            memory_after.web.activity_log, memory_before.web.activity_log,
            "a rejected native admission recorded connection activity in memory"
        );
        let durable_after = load_remote_machine_state()
            .expect("load durable native registration state after rejection");
        assert_eq!(
            durable_after.host.paired_clients, durable_before.host.paired_clients,
            "a rejected native admission persisted paired credentials"
        );
        assert_eq!(
            durable_after.host.web.activity_log, durable_before.host.web.activity_log,
            "a rejected native admission persisted connection activity"
        );
    }

    #[test]
    fn native_restart_rejects_a_worker_paused_before_registration() {
        let port = reserve_free_tcp_port();
        let mut config = RemoteHostConfig {
            enabled: false,
            bind_address: "127.0.0.1".to_string(),
            port,
            ..RemoteHostConfig::default()
        };
        let root = RemoteHostService::new(config.clone());
        let (native_lifecycle_tx, native_lifecycle_rx) = mpsc::sync_channel(1);
        *root
            .inner
            .native_lifecycle_test_hook
            .write()
            .expect("native lifecycle hook lock") = Some(Arc::new(move |event| {
            if event == super::NativeLifecycleTestEvent::ListenerStarted {
                native_lifecycle_tx
                    .send(())
                    .expect("native lifecycle observer should remain");
            }
        }));
        config.enabled = true;
        root.apply_config(config.clone());
        native_lifecycle_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("native listener should start");

        let (registration_entered_tx, registration_entered_rx) = mpsc::sync_channel(1);
        let (registration_release_tx, registration_release_rx) = mpsc::sync_channel(0);
        let registration_release_rx = Arc::new(Mutex::new(registration_release_rx));
        *root
            .inner
            .native_worker_registration_test_hook
            .write()
            .expect("native worker registration hook lock") = Some(Arc::new(move || {
            registration_entered_tx
                .send(())
                .expect("native registration observer should remain");
            registration_release_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv()
                .expect("native worker registration should be released");
        }));
        let (transition_tx, transition_rx) = mpsc::sync_channel(1);
        *root
            .inner
            .lifecycle_lock_acquired_test_hook
            .write()
            .expect("lifecycle transition hook lock") = Some(Arc::new(move || {
            transition_tx
                .send(())
                .expect("lifecycle transition observer should remain");
        }));
        let (worker_reaped_tx, worker_reaped_rx) = mpsc::sync_channel(1);
        *root
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("worker reaped hook lock") = Some(worker_reaped_tx);

        let raw_client = TcpStream::connect(("127.0.0.1", port))
            .expect("raw client should reach native listener");
        registration_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("listener should pause before native worker registration");

        let restart_service = root.clone();
        let (restart_done_tx, restart_done_rx) = mpsc::sync_channel(1);
        config.enabled = false;
        let restart = thread::spawn(move || {
            restart_service.apply_config(config);
            restart_done_tx
                .send(())
                .expect("restart observer should remain");
        });
        transition_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("restart should complete its lifecycle transition");
        restart_done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("bounded restart should return while registration is paused");
        restart.join().expect("restart worker should finish");

        registration_release_tx
            .send(())
            .expect("native worker registration should still be waiting");
        let reaped_worker = worker_reaped_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("deferred native listener should be reaped");
        assert_eq!(reaped_worker.name, "remote-native-listener");
        *root
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("worker reaped hook lock") = None;
        assert!(
            root.inner
                .native_connection_workers
                .lock()
                .expect("native connection workers lock")
                .is_empty(),
            "an old listener registered a native worker after restart drained its generation"
        );
        drop(raw_client);
    }

    #[test]
    fn poisoned_local_forward_entry_registry_retains_admitted_worker() {
        let manager = LocalPortForwardManager::new(sample_remote_client_handle("poisoned-entry"));
        let poison_inner = manager.inner.clone();
        let poisoned = thread::spawn(move || {
            let _guard = poison_inner
                .entries
                .lock()
                .expect("entry registry should start healthy");
            panic!("poison local-forward entry registry");
        })
        .join();
        assert!(poisoned.is_err(), "test did not poison the entry registry");

        let pool = Arc::new(RemoteWorkerAdmissionPool::new(1));
        let worker = RemoteWorker::try_spawn_with_pool(
            pool.clone(),
            "test-poisoned-local-forward-entry",
            None,
            || {},
        )
        .expect("test worker should be admitted");
        super::install_local_port_forward_entry(
            &manager.inner,
            49_152,
            super::LocalPortForwardEntry {
                scope_id: None,
                stop: None,
                worker: Some(worker),
                wakeup: None,
                retry_after_epoch_ms: 0,
            },
        );

        let mut entry = manager
            .inner
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&49_152)
            .expect("poison recovery must retain the admitted entry");
        entry
            .worker
            .take()
            .expect("retained entry must still own its worker")
            .join()
            .expect("retained worker should join");
        assert_eq!(pool.in_use(), 0, "joining must release its admission");
    }

    #[test]
    fn local_port_forward_manager_reports_busy_local_port() {
        let occupied_port = reserve_free_tcp_port();
        let _occupied = TcpListener::bind(("127.0.0.1", occupied_port))
            .expect("test should occupy a localhost port");
        let manager = LocalPortForwardManager::new(sample_remote_client_handle("client-1"));

        assert!(manager.sync_ports(&[occupied_port]));

        let state = manager
            .state_for(occupied_port)
            .expect("busy port should produce a status");
        assert!(!state.listener_active);
        assert!(state.local_port_busy);
        assert!(state
            .message
            .as_deref()
            .is_some_and(|message| message.contains("already in use")));
    }

    #[test]
    fn local_port_forward_shutdown_closes_a_real_forward_and_releases_its_port() {
        let _profile = TestProfileGuard::new("local-forward-real-shutdown");
        let host_port = reserve_free_tcp_port();
        let upstream = TcpListener::bind(("127.0.0.1", 0)).expect("upstream listener");
        let upstream_port = upstream.local_addr().expect("upstream address").port();
        let local_port = reserve_free_tcp_port();
        let mut host_config = RemoteHostConfig {
            enabled: true,
            bind_address: "127.0.0.1".to_string(),
            port: host_port,
            ..RemoteHostConfig::default()
        };
        let pair_token = host_config.pairing_token.clone();
        let host = RemoteHostService::new(host_config.clone());
        *host
            .inner
            .port_forward_connector_test_hook
            .write()
            .expect("forward connector hook lock") = Some(Arc::new(move |port| {
            if port != local_port {
                return Err(format!("unexpected forwarded port {port}"));
            }
            TcpStream::connect(("127.0.0.1", upstream_port))
                .map_err(|error| format!("test upstream connect failed: {error}"))
        }));

        let client = RemoteClientHandle::connect(
            "127.0.0.1",
            host_port,
            "Local forward integration client",
            ClientAuth::PairToken { token: pair_token },
            None,
        )
        .expect("remote client should connect");
        let manager = LocalPortForwardManager::new(client.client.clone());
        assert!(manager.sync_ports(&[local_port]));
        publish_live_managed_port(&host, local_port);

        let (upstream_closed_tx, upstream_closed_rx) = mpsc::sync_channel(1);
        let upstream_worker = thread::spawn(move || {
            let (mut socket, _) = upstream.accept().expect("forward should reach upstream");
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("upstream read timeout");
            let mut request = [0_u8; 4];
            socket
                .read_exact(&mut request)
                .expect("upstream should receive forwarded request");
            assert_eq!(&request, b"ping");
            socket.write_all(b"pong").expect("upstream response");
            socket
                .set_read_timeout(Some(Duration::from_millis(500)))
                .expect("upstream close deadline");
            let closed = read_proves_socket_closed(&mut socket);
            upstream_closed_tx
                .send(closed)
                .expect("upstream closure observer should remain");
        });

        let mut local_peer = TcpStream::connect(("127.0.0.1", local_port))
            .expect("local peer should reach forward listener");
        local_peer
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("local peer timeout");
        local_peer.write_all(b"ping").expect("local request");
        let mut response = [0_u8; 4];
        if let Err(error) = local_peer.read_exact(&mut response) {
            panic!(
                "local peer should receive upstream response: {error}; manager={:?}; host_note={:?}",
                manager.state_for(local_port),
                host.status().last_connection_note
            );
        }
        assert_eq!(&response, b"pong");

        let shutdown_started = Instant::now();
        manager.shutdown();
        assert!(
            shutdown_started.elapsed() < Duration::from_millis(500),
            "real local forward did not join within the lifecycle deadline"
        );
        assert!(
            upstream_closed_rx
                .recv_timeout(Duration::from_millis(500))
                .expect("upstream should observe forward cancellation"),
            "upstream socket remained open after local forward shutdown"
        );
        local_peer
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("local peer close deadline");
        assert!(
            read_proves_socket_closed(&mut local_peer),
            "local peer socket remained open after forward shutdown"
        );
        assert!(
            manager
                .inner
                .worker_registry
                .lock()
                .expect("forward worker registry lock")
                .is_empty(),
            "real local forward left lifecycle workers registered"
        );
        let rebound = TcpListener::bind(("127.0.0.1", local_port))
            .expect("local forward port should be reusable immediately after joined shutdown");
        drop(rebound);
        upstream_worker.join().expect("upstream worker should join");
        client.client.disconnect();
        host_config.enabled = false;
        host.apply_config(host_config);
    }

    #[test]
    fn local_port_forward_shutdown_joins_an_accepted_connection_worker() {
        let port = reserve_free_tcp_port();
        let manager = LocalPortForwardManager::new(sample_remote_client_handle("client-1"));
        let (connection_entered_tx, connection_entered_rx) = mpsc::sync_channel(1);
        let (connection_release_tx, connection_release_rx) = mpsc::sync_channel(0);
        let connection_release_rx = Arc::new(Mutex::new(connection_release_rx));
        let (worker_events_tx, worker_events_rx) = mpsc::channel();
        let shutdown_done_tx = worker_events_tx.clone();
        *manager
            .inner
            .connection_handler_test_hook
            .write()
            .expect("connection handler hook lock") = Some(Arc::new(move |_, _socket, _stop| {
            connection_entered_tx
                .send(())
                .expect("connection observer should remain");
            connection_release_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv_timeout(Duration::from_secs(3))
                .expect("connection should be released");
            worker_events_tx
                .send("connection-exited")
                .expect("connection exit observer should remain");
        }));

        assert!(manager.sync_ports(&[port]));
        let client = TcpStream::connect(("127.0.0.1", port))
            .expect("test client should reach local forward listener");
        connection_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("listener should accept the test connection");

        let shutdown_manager = manager.clone();
        let shutdown = thread::spawn(move || {
            shutdown_manager.shutdown();
            shutdown_done_tx
                .send("shutdown-finished")
                .expect("shutdown observer should remain");
        });
        connection_release_tx
            .send(())
            .expect("connection should still be waiting");
        assert_eq!(
            worker_events_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("connection worker should exit after release"),
            "connection-exited"
        );
        assert_eq!(
            worker_events_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("manager shutdown should finish after the connection worker exits"),
            "shutdown-finished"
        );
        shutdown.join().expect("shutdown worker should finish");
        drop(client);
    }

    #[test]
    fn local_port_forward_shutdown_rejects_a_connection_paused_before_registration() {
        let port = reserve_free_tcp_port();
        let manager = LocalPortForwardManager::new(sample_remote_client_handle("client-1"));
        let (lifecycle_tx, lifecycle_rx) = mpsc::sync_channel(2);
        let (registration_release_tx, registration_release_rx) = mpsc::sync_channel(0);
        let registration_release_rx = Arc::new(Mutex::new(registration_release_rx));
        *manager
            .inner
            .lifecycle_test_hook
            .write()
            .expect("local forward lifecycle hook lock") = Some(Arc::new(move |event| {
            lifecycle_tx
                .send(event)
                .expect("local forward lifecycle observer should remain");
            if event == LocalPortForwardLifecycleTestEvent::ConnectionAccepted {
                registration_release_rx
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .recv()
                    .expect("accepted connection registration should be released");
            }
        }));
        let (handler_started_tx, handler_started_rx) = mpsc::sync_channel(1);
        *manager
            .inner
            .connection_handler_test_hook
            .write()
            .expect("connection handler hook lock") = Some(Arc::new(move |_, _socket, _stop| {
            handler_started_tx
                .send(())
                .expect("connection handler observer should remain");
        }));

        assert!(manager.sync_ports(&[port]));
        let client = TcpStream::connect(("127.0.0.1", port))
            .expect("test client should reach local forward listener");
        assert_eq!(
            lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("listener should pause the accepted connection before registration"),
            LocalPortForwardLifecycleTestEvent::ConnectionAccepted
        );

        let shutdown_manager = manager.clone();
        let (shutdown_done_tx, shutdown_done_rx) = mpsc::sync_channel(1);
        let shutdown = thread::spawn(move || {
            shutdown_manager.shutdown();
            shutdown_done_tx
                .send(())
                .expect("shutdown observer should remain");
        });
        assert_eq!(
            lifecycle_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("shutdown should close connection acceptance"),
            LocalPortForwardLifecycleTestEvent::AcceptanceClosed
        );
        registration_release_tx
            .send(())
            .expect("accepted connection registration should still be waiting");
        shutdown_done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("shutdown should join the released listener");
        shutdown.join().expect("shutdown worker should finish");

        assert!(
            handler_started_rx.try_recv().is_err(),
            "shutdown allowed an accepted socket to register a new connection worker after acceptance closed"
        );
        assert!(
            manager
                .inner
                .worker_registry
                .lock()
                .expect("connection workers lock")
                .is_empty(),
            "shutdown left a late connection worker registered"
        );
        assert_eq!(
            manager.inner.worker_residue_count.load(Ordering::Acquire),
            0,
            "joined shutdown should not leave worker residue"
        );
        drop(client);
    }

    #[test]
    fn dropping_local_port_forward_manager_owns_listener_and_connection_workers() {
        let port = reserve_free_tcp_port();
        let manager = LocalPortForwardManager::new(sample_remote_client_handle("client-1"));
        let weak_inner = Arc::downgrade(&manager.inner);
        let (connection_entered_tx, connection_entered_rx) = mpsc::sync_channel(1);
        let (connection_release_tx, connection_release_rx) = mpsc::sync_channel(0);
        let connection_release_rx = Arc::new(Mutex::new(connection_release_rx));
        let (worker_events_tx, worker_events_rx) = mpsc::channel();
        let drop_done_tx = worker_events_tx.clone();
        *manager
            .inner
            .connection_handler_test_hook
            .write()
            .expect("connection handler hook lock") = Some(Arc::new(move |_, _socket, _stop| {
            connection_entered_tx
                .send(())
                .expect("connection observer should remain");
            connection_release_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv_timeout(Duration::from_secs(3))
                .expect("connection should be released");
            worker_events_tx
                .send("connection-exited")
                .expect("connection exit observer should remain");
        }));

        assert!(manager.sync_ports(&[port]));
        let client = TcpStream::connect(("127.0.0.1", port))
            .expect("test client should reach local forward listener");
        connection_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("listener should accept the test connection");

        let drop_thread = thread::spawn(move || {
            drop(manager);
            drop_done_tx
                .send("drop-finished")
                .expect("drop observer should remain");
        });
        connection_release_tx
            .send(())
            .expect("connection should still be waiting");
        assert_eq!(
            worker_events_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("connection worker should exit after release"),
            "connection-exited"
        );
        assert_eq!(
            worker_events_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("manager drop should finish after the connection exits"),
            "drop-finished"
        );
        drop_thread.join().expect("drop worker should finish");
        drop(client);

        assert!(
            weak_inner.upgrade().is_none(),
            "manager workers retained a strong lifecycle cycle after teardown"
        );
    }

    #[test]
    fn dropping_local_port_forward_manager_stops_listener_during_acceptance_callback() {
        let port = reserve_free_tcp_port();
        let manager = LocalPortForwardManager::new(sample_remote_client_handle("client-1"));
        let weak_inner = Arc::downgrade(&manager.inner);
        let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let release_rx = Arc::new(Mutex::new(release_rx));
        let (shutdown_started_tx, shutdown_started_rx) = mpsc::sync_channel(1);
        *manager
            .inner
            .lifecycle_test_hook
            .write()
            .expect("local forward lifecycle hook lock") =
            Some(Arc::new(move |event| match event {
                LocalPortForwardLifecycleTestEvent::ConnectionAccepted => {
                    accepted_tx
                        .send(())
                        .expect("acceptance observer should remain");
                    release_rx
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .recv_timeout(Duration::from_secs(3))
                        .expect("listener acceptance callback should be released");
                }
                LocalPortForwardLifecycleTestEvent::AcceptanceClosed => {
                    shutdown_started_tx
                        .send(())
                        .expect("shutdown observer should remain");
                }
            }));

        assert!(manager.sync_ports(&[port]));
        let client = TcpStream::connect(("127.0.0.1", port))
            .expect("test client should reach local forward listener");
        accepted_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("listener should enter the acceptance callback");

        let (drop_done_tx, drop_done_rx) = mpsc::sync_channel(1);
        let drop_thread = thread::spawn(move || {
            drop(manager);
            drop_done_tx.send(()).expect("drop observer should remain");
        });
        shutdown_started_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("manager drop should initiate listener shutdown while callback is blocked");
        release_tx
            .send(())
            .expect("acceptance callback should still be waiting");
        drop_done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("manager drop should join the listener after callback release");
        drop_thread.join().expect("drop worker should finish");
        drop(client);

        assert!(
            weak_inner.upgrade().is_none(),
            "manager drop left the listener holding the lifecycle owner"
        );
    }

    #[test]
    fn apply_local_terminal_resize_updates_snapshot_session_view_metadata() {
        let handle = sample_remote_client_handle("client-1");
        let mut snapshot = handle.latest_snapshot().expect("snapshot should exist");
        let mut view = session_view("alpha");
        view.screen.rows = 40;
        view.screen.cols = 120;
        view.screen.total_lines = 200;
        view.screen.history_size = 160;
        view.screen.display_offset = 99;
        snapshot
            .runtime_state
            .sessions
            .insert("alpha".to_string(), view.runtime.clone());
        snapshot
            .session_views
            .insert("alpha".to_string(), view.clone());
        if let Ok(mut latest) = handle.inner.latest_snapshot.write() {
            *latest = Some(snapshot);
        }

        let dimensions = SessionDimensions {
            cols: 90,
            rows: 20,
            cell_width: 8,
            cell_height: 18,
        };
        handle.apply_local_terminal_resize("alpha", dimensions);

        let snapshot = handle.latest_snapshot().expect("snapshot should exist");
        let updated = snapshot
            .session_views
            .get("alpha")
            .expect("session view should remain present");
        assert_eq!(updated.runtime.dimensions, dimensions);
        assert_eq!(updated.screen.rows, 20);
        assert_eq!(updated.screen.cols, 90);
        assert_eq!(updated.screen.history_size, 180);
        assert_eq!(updated.screen.display_offset, 99);
    }

    #[test]
    fn browser_semantic_delivery_is_exact_once_and_never_uses_native_messages() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let key = StableSessionKey::from_tab("semantic-tab");
        let (native_tx, native_rx) = mpsc::channel();
        let web_tx = BrowserOutboundSender::detached_for_test(8, 4 * 1024 * 1024);
        let observed_web = web_tx.clone();
        let mut client = test_connected_client("browser", native_tx, Some(web_tx));
        client.semantic_cursors.insert(key.clone(), 0);
        service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .insert(1, client);

        let published = publish_semantic_event(
            &service.inner,
            semantic_status_draft(key.clone(), "ready", 1),
        );
        assert!(deliver_live_semantic_events(&service.inner));
        let queued_after_first = observed_web.queued_bytes();
        assert!(queued_after_first > 0, "live browser event was queued");
        assert!(
            native_rx.try_recv().is_err(),
            "browser-only frames must not enter ServerMessage"
        );

        assert!(deliver_live_semantic_events(&service.inner));
        assert_eq!(
            observed_web.queued_bytes(),
            queued_after_first,
            "a committed semantic cursor must not be delivered twice"
        );
        let cursor = service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .get(&1)
            .and_then(|client| client.semantic_cursors.get(&key))
            .copied();
        assert_eq!(cursor, Some(published.sequence));
    }

    #[test]
    fn semantic_cursor_rollover_disconnects_for_a_clean_resume() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        *service
            .inner
            .semantic_journals
            .lock()
            .expect("journal lock") = SemanticJournalStore::with_limits(JournalLimits {
            canonical_events: 1,
            canonical_bytes: 1024 * 1024,
            verbose_events: 1,
            verbose_bytes: 1024 * 1024,
        });
        let key = StableSessionKey::from_tab("rollover-tab");
        let (native_tx, _native_rx) = mpsc::channel();
        let web_tx = BrowserOutboundSender::detached_for_test(8, 4 * 1024 * 1024);
        let observed_web = web_tx.clone();
        let mut client = test_connected_client("browser", native_tx, Some(web_tx));
        client.semantic_cursors.insert(key.clone(), 0);
        service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .insert(1, client);

        let first = publish_semantic_event(
            &service.inner,
            semantic_status_draft(key.clone(), "first", 1),
        );
        service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .get_mut(&1)
            .expect("browser client")
            .semantic_cursors
            .insert(key.clone(), first.sequence);
        let second = publish_semantic_event(
            &service.inner,
            semantic_status_draft(key.clone(), "second", 2),
        );

        assert!(deliver_live_semantic_events(&service.inner));
        let queued_after_rollover = observed_web.queued_bytes();
        assert!(queued_after_rollover > 0, "disconnect frame was queued");
        assert!(
            !observed_web.is_active(),
            "rolled-over browser stays fenced"
        );
        assert!(!service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .contains_key(&1));
        assert!(second.sequence > first.sequence);
        assert!(deliver_live_semantic_events(&service.inner));
        assert_eq!(observed_web.queued_bytes(), queued_after_rollover);
    }

    #[test]
    fn saturated_semantic_browser_is_dropped_without_blocking_fanout() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let key = StableSessionKey::from_tab("slow-tab");
        let (native_tx, _native_rx) = mpsc::channel();
        let web_tx = BrowserOutboundSender::detached_for_test(1, 4 * 1024 * 1024);
        web_tx
            .try_send(WsOutbound::Pong)
            .expect("prefill bounded channel");
        let mut client = test_connected_client("slow-browser", native_tx, Some(web_tx));
        client.semantic_cursors.insert(key.clone(), 0);
        service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .insert(1, client);
        publish_semantic_event(&service.inner, semantic_status_draft(key, "ready", 1));

        let started = Instant::now();
        assert!(deliver_live_semantic_events(&service.inner));
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(
            !service
                .inner
                .clients
                .lock()
                .expect("clients lock")
                .contains_key(&1),
            "a saturated browser must not retain an unbounded backlog"
        );
    }

    #[test]
    fn pty_output_does_not_wait_for_blocked_semantic_fanout() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let mut app = AppState::default();
        app.open_tabs.push(SessionTab {
            id: "semantic-tab".to_string(),
            tab_type: TabType::Claude,
            pty_session_id: Some("semantic-runtime".to_string()),
            provider_session_id: None,
            ..SessionTab::default()
        });
        let mut runtime = RuntimeState::default();
        let mut session = SessionRuntimeState::new(
            "semantic-runtime",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.session_kind = SessionKind::Claude;
        session.tab_id = Some("semantic-tab".to_string());
        runtime.sessions.insert(session.session_id.clone(), session);
        service.update_snapshot(app, runtime, HashMap::new());
        service.push_session_output("semantic-runtime", b"first\n".to_vec());

        let key = StableSessionKey::from_tab("semantic-tab");
        let (native_tx, _native_rx) = mpsc::channel();
        let web_tx = BrowserOutboundSender::detached_for_test(8, 4 * 1024 * 1024);
        let mut client = test_connected_client("browser", native_tx, Some(web_tx));
        client.semantic_cursors.insert(key, 0);
        service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .insert(1, client);

        let (entered_tx, entered_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let hook_release = release.clone();
        *service
            .inner
            .semantic_delivery_test_hook
            .write()
            .expect("delivery hook lock") = Some(Arc::new(move || {
            entered_tx.send(()).expect("delivery observer");
            let (lock, cvar) = &*hook_release;
            let mut released = lock.lock().expect("delivery gate lock");
            while !*released {
                released = cvar.wait(released).expect("delivery gate wait");
            }
        }));

        let delivery_inner = service.inner.clone();
        let delivery = thread::spawn(move || deliver_live_semantic_events(&delivery_inner));
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("fanout reached blocking hook");

        let started = Instant::now();
        service.push_session_output("semantic-runtime", b"second\n".to_vec());
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "PTY publication waited for browser fanout"
        );

        let (lock, cvar) = &*release;
        *lock.lock().expect("delivery gate lock") = true;
        cvar.notify_all();
        assert!(delivery.join().expect("delivery thread"));
    }

    #[test]
    fn semantic_only_revision_still_wakes_the_browser_snapshot_path() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (native_tx, native_rx) = mpsc::channel();
        let web_tx = BrowserOutboundSender::detached_for_test(8, 4 * 1024 * 1024);
        let observed_web = web_tx.clone();
        let client = test_connected_client("browser", native_tx, Some(web_tx));
        service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .insert(1, client);
        publish_semantic_event(
            &service.inner,
            semantic_status_draft(StableSessionKey::from_tab("revision-tab"), "ready", 1),
        );

        service.inner.stop_flag.store(false, Ordering::SeqCst);
        let broadcaster_inner = Arc::downgrade(&service.inner);
        let broadcaster_signal = service.inner.broadcaster_signal.clone();
        let generation = service
            .inner
            .native_runtime_generation
            .load(Ordering::Acquire);
        let broadcaster = thread::spawn(move || {
            run_broadcaster(broadcaster_inner, broadcaster_signal, generation)
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while observed_web.queued_bytes() == 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        service.inner.stop_flag.store(true, Ordering::SeqCst);
        broadcaster.join().expect("broadcaster thread");
        assert!(observed_web.queued_bytes() > 0, "browser delta was queued");
        assert!(
            native_rx.try_recv().is_err(),
            "browser delta must not use the native MessagePack lane"
        );
    }

    #[test]
    fn native_server_message_pong_messagepack_shape_is_unchanged() {
        let encoded = rmp_serde::encode::to_vec_named(&ServerMessage::Pong)
            .expect("native pong serialization");
        assert_eq!(
            encoded,
            vec![0x81, 0xa4, b't', b'y', b'p', b'e', 0xa4, b'p', b'o', b'n', b'g']
        );
    }

    #[test]
    fn restart_drain_removes_web_state_but_preserves_native_client() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (web_native_tx, _web_native_rx) = mpsc::channel();
        let web_tx = BrowserOutboundSender::detached_for_test(8, 4 * 1024 * 1024);
        let (native_tx, _native_rx) = mpsc::channel();
        let web_client = test_connected_client("browser", web_native_tx, Some(web_tx));
        let native_client = test_connected_client("native", native_tx, None);
        {
            let mut clients = service.inner.clients.lock().expect("clients lock");
            clients.insert(1, web_client);
            clients.insert(2, native_client);
        }
        service
            .inner
            .web_control
            .lock()
            .expect("web control lock")
            .writer_leases_mut()
            .acquire(1, "browser", "tab", 1_000)
            .expect("browser lease");
        *service
            .inner
            .controller_client_id
            .write()
            .expect("controller lock") = Some("browser".to_string());

        drain_web_clients_for_restart(&service.inner);

        let clients = service.inner.clients.lock().expect("clients lock");
        assert!(!clients.contains_key(&1));
        assert!(
            clients.contains_key(&2),
            "native connection must survive web restart"
        );
        drop(clients);
        assert!(service
            .inner
            .web_control
            .lock()
            .expect("web control lock")
            .writer_leases()
            .peek()
            .is_none());
        assert!(service
            .inner
            .controller_client_id
            .read()
            .expect("controller lock")
            .is_none());
    }

    #[test]
    fn restart_drain_never_clears_the_real_native_controller() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (web_native_tx, _web_native_rx) = mpsc::channel();
        let web_tx = BrowserOutboundSender::detached_for_test(8, 4 * 1024 * 1024);
        let (native_tx, _native_rx) = mpsc::channel();
        {
            let mut clients = service.inner.clients.lock().expect("clients lock");
            clients.insert(
                1,
                test_connected_client("browser", web_native_tx, Some(web_tx)),
            );
            clients.insert(2, test_connected_client("native", native_tx, None));
        }
        *service
            .inner
            .controller_client_id
            .write()
            .expect("controller lock") = Some("native".to_string());

        drain_web_clients_for_restart(&service.inner);

        assert_eq!(
            service
                .inner
                .controller_client_id
                .read()
                .expect("controller lock")
                .as_deref(),
            Some("native")
        );
        assert!(service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .contains_key(&2));
    }

    fn semantic_status_draft(
        stable_session_key: StableSessionKey,
        state: &str,
        occurred_at_epoch_ms: u64,
    ) -> SemanticEventDraft {
        SemanticEventDraft {
            stable_session_key,
            occurred_at_epoch_ms,
            source: SemanticSource::System,
            kind: SemanticEventKind::Status {
                state: state.to_string(),
                detail: None,
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: None,
        }
    }

    fn test_connected_client(
        client_id: &str,
        sender: mpsc::Sender<ServerMessage>,
        web_sender: Option<BrowserOutboundSender>,
    ) -> ConnectedRemoteClient {
        let web_tombstone = web_sender.as_ref().map(BrowserOutboundSender::tombstone);
        let sender = web_sender.is_none().then_some(Arc::new(sender));
        ConnectedRemoteClient {
            client_id: client_id.to_string(),
            sender,
            web_sender,
            web_tombstone,
            semantic_cursors: HashMap::new(),
            subscribed_session_ids: HashSet::new(),
            bootstrapped_session_ids: HashSet::new(),
            bootstrap_pending_session_ids: HashSet::new(),
            focused_session_id: None,
            last_app_hash: 0,
            last_runtime_hash: 0,
            last_port_hash: 0,
            last_controller_client_id: None,
            last_you_have_control: false,
            last_snapshot_revision: 0,
        }
    }

    #[test]
    fn native_post_registration_cleanup_preserves_newer_same_client_registration() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let connection_id = 73;
        let (old_sender, _old_receiver) = mpsc::channel();
        let old_registration = test_connected_client("same-native-client", old_sender, None);
        let stale_cleanup = super::client_delivery_target(&old_registration)
            .expect("native registration should have a delivery target");
        service
            .inner
            .clients
            .lock()
            .expect("clients lock")
            .insert(connection_id, old_registration);

        let (replacement_sender, _replacement_receiver) = mpsc::channel();
        service.inner.clients.lock().expect("clients lock").insert(
            connection_id,
            test_connected_client("same-native-client", replacement_sender, None),
        );

        super::revoke_failed_delivery(&service.inner, connection_id, stale_cleanup);

        assert!(
            service
                .inner
                .clients
                .lock()
                .expect("clients lock")
                .contains_key(&connection_id),
            "stale post-registration cleanup removed a newer native registration"
        );
    }

    fn session_view(session_id: &str) -> TerminalSessionView {
        TerminalSessionView {
            runtime: SessionRuntimeState::new(
                session_id.to_string(),
                PathBuf::from("C:\\Code"),
                SessionDimensions::default(),
                TerminalBackend::default(),
            ),
            screen: TerminalScreenSnapshot::default(),
        }
    }

    fn reserve_free_tcp_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0))
            .expect("should bind ephemeral port")
            .local_addr()
            .expect("listener should have a local address")
            .port()
    }

    fn wait_for<F>(mut predicate: F, timeout: Duration, context: &str)
    where
        F: FnMut() -> bool,
    {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if predicate() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("{context}");
    }

    fn managed_server_state(port: u16) -> AppState {
        let mut state = AppState::default();
        state.config.projects = vec![crate::models::Project {
            id: "project-web".to_string(),
            name: "Web".to_string(),
            folders: vec![crate::models::ProjectFolder {
                id: "folder-web".to_string(),
                name: "web".to_string(),
                commands: vec![crate::models::RunCommand {
                    id: "command-web".to_string(),
                    label: "web".to_string(),
                    port: Some(port),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }];
        state
    }

    fn managed_server_runtime(command_id: &str, pid: u32, port: u16) -> RuntimeState {
        let mut runtime = RuntimeState::default();
        let mut session = SessionRuntimeState::new(
            command_id.to_string(),
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.configure_server(crate::state::ServerLaunchSpec {
            command_id: command_id.to_string(),
            project_id: "project-web".to_string(),
            port: Some(port),
            cwd: PathBuf::from("."),
            program: "test-server".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            auto_restart: false,
            log_file_path: None,
        });
        session.status = crate::state::SessionStatus::Running;
        session.pid = Some(pid);
        session.resources.process_ids.push(pid);
        runtime.sessions.insert(command_id.to_string(), session);
        runtime
    }

    fn publish_live_managed_port(service: &RemoteHostService, port: u16) {
        let inventory = crate::services::ports_service::PortInventory::new();
        let live_snapshot = inventory
            .refresh(&[port])
            .expect("strict live port inventory should complete");
        assert!(live_snapshot.is_valid(), "live port snapshot must validate");
        let live_listener = live_snapshot
            .observation(port)
            .expect("live port observation")
            .listeners()
            .first()
            .expect("live listener identity");
        let process_id = live_listener.pid();
        let executable = live_listener
            .canonical_executable()
            .expect("strict live listener must include executable proof")
            .to_path_buf();
        let managed_identity = crate::process::identity::ManagedProcessIdentity::new(
            crate::process::identity::ManagedProcessId::new(
                process_id,
                live_listener.creation_time_100ns(),
            )
            .expect("live listener process identity should be valid"),
            executable,
        )
        .expect("live listener executable should canonicalize");
        let resource = ResourceFence::new(ResourceId::new(), 1);
        let managed = crate::process::ports::ManagedResourceSnapshot::new(
            crate::process::registry::ManagedProcessFence::new(
                resource,
                crate::process::identity::ProcessOwner::Host,
                managed_identity.clone(),
            ),
            crate::process::registry::ManagedProcessState::Running,
            vec![managed_identity],
            crate::process::ports::RegistryMembershipSnapshot::valid(
                1,
                1,
                Instant::now(),
                Duration::from_secs(5),
            ),
        );
        let observed_at = live_snapshot.observed_at();
        let live_status = crate::process::ports::project_port_status_from_snapshot_at(
            &crate::process::ports::PortTarget::new(
                port,
                resource,
                crate::process::ports::ManagedPortHealth::Ready,
            ),
            &live_snapshot,
            Some(&managed),
            Instant::now(),
            observed_at
                .checked_add(crate::process::ports::DEFAULT_FREE_PROOF_MAX_AGE)
                .expect("live snapshot deadline should fit"),
        );
        assert_eq!(
            live_status.kind(),
            crate::process::ports::PortStatusKind::ManagedHealthy,
            "test listener must project as live managed authority: {live_status:?}"
        );
        let capability = Arc::new(crate::process::ports::test_capability_from_snapshot(
            managed,
        ));
        let authority = RemotePortAuthority::from_rich(&live_status, now_epoch_ms())
            .with_snapshot_metadata(
                live_snapshot.publication_sequence(),
                capability.snapshot().membership_revision(),
                capability.snapshot().observation_sequence(),
            )
            .with_session_id("command-web")
            .with_managed_capability(capability.as_ref());
        let legacy_status = PortStatus {
            port,
            in_use: true,
            pid: Some(process_id),
            process_name: None,
        };
        service.update_snapshot_parts_with_authorities(
            Some(managed_server_state(port)),
            Some(managed_server_runtime("command-web", process_id, port)),
            Some(HashMap::from([(port, legacy_status)])),
            Some(HashMap::from([(port, authority)])),
        );
        service.update_managed_port_capabilities(HashMap::from([(port, capability)]));
    }

    fn sample_remote_client_handle(client_id: &str) -> RemoteClientHandle {
        let (tx, _rx) = mpsc::channel();
        let inner = Arc::new(RemoteClientInner {
            pending: Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(1),
            latest_snapshot: RwLock::new(Some(RemoteWorkspaceSnapshot {
                server_id: "host-1".to_string(),
                ..RemoteWorkspaceSnapshot::default()
            })),
            session_replicas: RwLock::new(HashMap::new()),
            disconnected_message: RwLock::new(None),
            snapshot_revision: AtomicU64::new(1),
            session_stream_revision: AtomicU64::new(1),
            latency: RwLock::new(RemoteLatencyStats::default()),
            pending_paint_received_at_epoch_ms: AtomicU64::new(0),
            pending_notification_count: AtomicU64::new(0),
            client_id: client_id.to_string(),
            client_token: "token-1".to_string(),
            server_id: "host-1".to_string(),
            certificate_fingerprint: "fingerprint-1".to_string(),
            address: "127.0.0.1".to_string(),
            port: 43871,
            #[cfg(test)]
            reader_exit_test_hook: RwLock::new(None),
        });
        RemoteClientHandle {
            connection: Arc::new(super::RemoteClientConnectionOwner {
                outgoing: tx,
                socket_wakeup: Mutex::new(None),
                reader: Mutex::new(None),
                inner: Arc::downgrade(&inner),
            }),
            inner,
        }
    }
}
