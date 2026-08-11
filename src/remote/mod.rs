mod access_log;
mod client_pool;
pub mod presentation;
mod transport;
pub mod web;

pub use access_log::{RemoteAccessActivityEvent, RemoteAccessActivityKind, RemoteAccessSource};
pub use client_pool::RemoteClientPool;
pub use web::{PairedWebClient, WebConfig, WebListenerHandle};

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
use crate::git::git_service::{
    AiCommitMessage, DeviceCodeResponse, GitBranch, GitDiffResult, GitLogEntry, GitStatusResult,
};
use crate::models::{
    PortStatus, Project, ProjectFolder, RootScanEntry, RunCommand, SSHConnection, ScanResult,
    Settings, TabType,
};
use crate::persistence::{self, PersistenceError};
use crate::process::ports::{PortStatus as RichPortStatus, PortStatusKind as RichPortStatusKind};
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

pub const PROTOCOL_VERSION: u32 = 5;
const REMOTE_FILE_NAME: &str = "remote.json";
const SNAPSHOT_BROADCAST_INTERVAL: Duration = Duration::from_millis(33);
const IDLE_BROADCAST_INTERVAL: Duration = Duration::from_millis(250);
const PENDING_BOOTSTRAP_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const REMOTE_WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
const REMOTE_CALLBACK_TIMEOUT: Duration = Duration::from_millis(500);
const PORT_FORWARD_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
pub(crate) const AI_STARTUP_REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const GIT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const REMOTE_ACCESS_LOG_LIMIT: usize = 100;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const MAX_OUTBOUND_MESSAGES_PER_TICK: usize = 128;
pub(crate) const MAX_PENDING_REMOTE_REQUESTS: usize = 256;
const MAX_CONCURRENT_REMOTE_HOST_WORK: usize = 8;
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
            web: WebConfig::default(),
        };
        let _ = transport::ensure_host_tls_material(&mut config);
        config
    }
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RemotePortAuthorityKind {
    Managed,
    ProvenExternal,
    Unknown,
    ProbeError,
    Free,
    Occupied,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteListenerIdentity {
    pub pid: u32,
    pub creation_time_100ns: u64,
    /// The executable path is intentionally not sent over the remote wire.
    /// This bit records that the local host captured and canonicalized it.
    pub executable_proven: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemotePortAuthority {
    pub port: u16,
    pub kind: RemotePortAuthorityKind,
    pub resource: Option<ResourceFence>,
    pub listeners: Vec<RemoteListenerIdentity>,
    pub membership_revision: u64,
    pub observation_sequence: u64,
    pub publication_sequence: u64,
    pub observed_at_epoch_ms: u64,
    pub freshness_deadline_epoch_ms: u64,
    pub error: Option<String>,
}

impl RemotePortAuthority {
    pub fn kind(&self) -> RemotePortAuthorityKind {
        self.kind
    }

    pub fn from_rich(status: &RichPortStatus, now_epoch_ms: u64) -> Self {
        let kind = match status.kind() {
            RichPortStatusKind::ManagedHealthy | RichPortStatusKind::ManagedUnready => {
                RemotePortAuthorityKind::Managed
            }
            RichPortStatusKind::ProvenExternal => RemotePortAuthorityKind::ProvenExternal,
            RichPortStatusKind::ProbeError => RemotePortAuthorityKind::ProbeError,
            RichPortStatusKind::Occupied => RemotePortAuthorityKind::Occupied,
            RichPortStatusKind::Stopped => RemotePortAuthorityKind::Free,
            RichPortStatusKind::Starting | RichPortStatusKind::Unknown => {
                RemotePortAuthorityKind::Unknown
            }
        };
        let resource = (kind == RemotePortAuthorityKind::Managed).then_some(status.resource);
        Self {
            port: status.port,
            kind,
            resource,
            listeners: status
                .listeners()
                .iter()
                .map(|listener| RemoteListenerIdentity {
                    pid: listener.pid(),
                    creation_time_100ns: listener.creation_time_100ns(),
                    executable_proven: listener.has_executable_proof(),
                })
                .collect(),
            membership_revision: 0,
            observation_sequence: 0,
            publication_sequence: 0,
            observed_at_epoch_ms: now_epoch_ms,
            freshness_deadline_epoch_ms: now_epoch_ms
                .saturating_add(REMOTE_PORT_AUTHORITY_MAX_AGE_MS),
            error: status.error().map(str::to_string),
        }
    }

    pub fn with_snapshot_metadata(
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

    pub fn is_fresh_at(&self, now_epoch_ms: u64) -> bool {
        self.publication_sequence > 0
            && self.observed_at_epoch_ms <= now_epoch_ms
            && now_epoch_ms.saturating_sub(self.observed_at_epoch_ms)
                <= REMOTE_PORT_AUTHORITY_MAX_AGE_MS
            && self.freshness_deadline_epoch_ms >= self.observed_at_epoch_ms
            && now_epoch_ms <= self.freshness_deadline_epoch_ms
    }

    pub fn has_exact_managed_fence_for(
        &self,
        requested_port: u16,
        session: &SessionRuntimeState,
    ) -> bool {
        if self.kind != RemotePortAuthorityKind::Managed
            || self.port != requested_port
            || self.resource.is_none()
            || self
                .resource
                .is_some_and(|resource| resource.runtime_generation == 0)
            || self.membership_revision == 0
            || self.observation_sequence == 0
            || !self
                .listeners
                .iter()
                .all(|listener| listener.executable_proven)
        {
            return false;
        }
        let Some(pid) = session.pid else {
            return false;
        };
        self.listeners
            .iter()
            .any(|listener| listener.pid == pid && listener.creation_time_100ns != 0)
    }
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
    pub client_id: String,
    pub action: RemoteAction,
    pub response: Option<mpsc::Sender<RemoteActionResult>>,
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
fn lock_new_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
    lock_remote_state_file_permissions(path)
}

#[cfg(windows)]
fn lock_new_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
    lock_remote_state_file_permissions(path)
}

#[cfg(not(any(unix, windows)))]
fn lock_new_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
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
fn verify_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
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
fn verify_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
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
fn verify_remote_state_file_permissions(path: &Path) -> std::io::Result<()> {
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

fn persist_host_config_snapshot(config: &RemoteHostConfig) -> Result<(), PersistenceError> {
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
) -> Result<Option<T>, String> {
    let _update_guard = inner
        .config_update_lock
        .lock()
        .map_err(|_| "host config update unavailable".to_string())?;
    let Some((result, snapshot, previous)) = ({
        let Ok(mut config) = inner.config.write() else {
            return Err("host config unavailable".to_string());
        };
        if !condition(&config) {
            None
        } else {
            let previous = config.clone();
            let result = mutate(&mut config);
            Some((result, config.clone(), previous))
        }
    }) else {
        return Ok(None);
    };

    if let Err(error) = persist_host_config_snapshot(&snapshot) {
        if let Ok(mut config) = inner.config.write() {
            *config = previous;
        }
        return Err(error.to_string());
    }

    bump_host_config_revision(inner);
    Ok(Some(result))
}

pub(crate) fn mutate_host_config<T>(
    inner: &Arc<RemoteHostInner>,
    mutate: impl FnOnce(&mut RemoteHostConfig) -> T,
) -> Result<T, String> {
    let _update_guard = inner
        .config_update_lock
        .lock()
        .map_err(|_| "host config update unavailable".to_string())?;
    let (result, snapshot, previous) = {
        let Ok(mut config) = inner.config.write() else {
            return Err("host config unavailable".to_string());
        };
        let previous = config.clone();
        let result = mutate(&mut config);
        (result, config.clone(), previous)
    };

    if let Err(error) = persist_host_config_snapshot(&snapshot) {
        if let Ok(mut config) = inner.config.write() {
            *config = previous;
        }
        return Err(error.to_string());
    }

    bump_host_config_revision(inner);
    Ok(result)
}

fn append_native_connection_activity(
    config: &mut RemoteHostConfig,
    client_id: String,
    label: String,
    ip_address: Option<String>,
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
            event_at_epoch_ms: Some(now_epoch_ms()),
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

struct RemoteWorker {
    name: String,
    completion_rx: mpsc::Receiver<()>,
    handle: Option<thread::JoinHandle<()>>,
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
    fn spawn(
        name: impl Into<String>,
        done: Option<Arc<AtomicBool>>,
        job: impl FnOnce() + Send + 'static,
    ) -> Self {
        let name = name.into();
        let (completion_tx, completion_rx) = mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name(name.clone())
            .spawn(move || {
                let _completion = RemoteWorkerCompletion {
                    completion_tx: Some(completion_tx),
                    done,
                };
                job();
            })
            .unwrap_or_else(|error| panic!("could not spawn remote worker {name}: {error}"));
        Self {
            name,
            completion_rx,
            handle: Some(handle),
        }
    }
}

struct NativeConnectionWorker {
    generation: u64,
    done: Arc<AtomicBool>,
    socket_wakeup: Option<TcpStream>,
    // Keep admission visible in the host reference count, but release this
    // hold before waiting on the worker so a stalled TLS handshake cannot
    // retain the stopped host runtime.
    runtime_hold: Option<Arc<RemoteHostInner>>,
    worker: RemoteWorker,
}

struct DeferredRemoteWorker {
    name: String,
    handle: thread::JoinHandle<()>,
    owner: DeferredRemoteWorkerOwner,
}

enum DeferredRemoteWorkerOwner {
    Host(Weak<RemoteHostInner>),
    LocalPortForward {
        inner: Weak<LocalPortForwardManagerInner>,
        port: u16,
    },
    Unowned,
}

struct RemoteWorkerReaper {
    sender: mpsc::Sender<DeferredRemoteWorker>,
    _handle: Mutex<Option<thread::JoinHandle<()>>>,
}

static REMOTE_WORKER_REAPER: OnceLock<RemoteWorkerReaper> = OnceLock::new();
static REMOTE_WORKER_REAPER_SIGNAL: OnceLock<Arc<(Mutex<u64>, Condvar)>> = OnceLock::new();

fn remote_worker_reaper_signal() -> &'static Arc<(Mutex<u64>, Condvar)> {
    REMOTE_WORKER_REAPER_SIGNAL.get_or_init(|| Arc::new((Mutex::new(0), Condvar::new())))
}

fn notify_remote_worker_reaper() {
    let signal = remote_worker_reaper_signal();
    if let Ok(mut sequence) = signal.0.lock() {
        *sequence = sequence.wrapping_add(1);
        signal.1.notify_one();
    }
}

fn remote_worker_reaper() -> &'static RemoteWorkerReaper {
    REMOTE_WORKER_REAPER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<DeferredRemoteWorker>();
        let signal = remote_worker_reaper_signal().clone();
        let handle = thread::Builder::new()
            .name("remote-worker-reaper".to_string())
            .spawn(move || {
                let mut pending = Vec::<DeferredRemoteWorker>::new();
                let mut observed_sequence = 0_u64;
                loop {
                    while let Ok(worker) = receiver.try_recv() {
                        pending.push(worker);
                    }

                    if pending.is_empty() {
                        match receiver.try_recv() {
                            Ok(worker) => pending.push(worker),
                            Err(mpsc::TryRecvError::Disconnected) => break,
                            Err(mpsc::TryRecvError::Empty) => {}
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

                    let guard = signal
                        .0
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if *guard == observed_sequence {
                        let guard = signal
                            .1
                            .wait(guard)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        observed_sequence = *guard;
                    } else {
                        observed_sequence = *guard;
                    }
                }
            })
            .expect("remote worker reaper should start");
        RemoteWorkerReaper {
            sender,
            _handle: Mutex::new(Some(handle)),
        }
    })
}

fn enqueue_deferred_remote_worker(worker: DeferredRemoteWorker) {
    remote_worker_reaper()
        .sender
        .send(worker)
        .expect("remote worker reaper should remain available");
    notify_remote_worker_reaper();
}

fn finish_deferred_remote_worker(worker: DeferredRemoteWorker) {
    let DeferredRemoteWorker {
        name,
        handle,
        owner,
    } = worker;
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
                #[cfg(test)]
                if let Some(hook) = inner
                    .worker_reaped_test_hook
                    .read()
                    .ok()
                    .and_then(|slot| slot.clone())
                {
                    hook(&name);
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
    enqueue_deferred_remote_worker(DeferredRemoteWorker {
        name: worker.name,
        handle,
        owner: DeferredRemoteWorkerOwner::Host(Arc::downgrade(inner)),
    });
}

fn settle_remote_worker(inner: &Arc<RemoteHostInner>, mut worker: RemoteWorker, deadline: Instant) {
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
            let input_handles = self
                .inner
                .web_input_executor
                .take_unfinished_worker_handles();
            if !input_handles.is_empty() {
                let worker = RemoteWorker::spawn("remote-web-input-residue", None, move || {
                    for handle in input_handles {
                        let _ = handle.join();
                    }
                });
                defer_remote_worker(&self.inner, worker);
            }
            let request_handles = self
                .inner
                .web_request_executor
                .take_unfinished_worker_handles();
            if !request_handles.is_empty() {
                let worker = RemoteWorker::spawn("remote-web-request-residue", None, move || {
                    for handle in request_handles {
                        let _ = handle.join();
                    }
                });
                defer_remote_worker(&self.inner, worker);
            }
        }

        if let Some(listener) = web_listener {
            let worker = RemoteWorker::spawn("remote-web-shutdown", None, move || {
                listener.shutdown();
            });
            settle_remote_worker(&self.inner, worker, deadline);
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
    if let Ok(mut sequence) = inner.broadcaster_signal_lock.lock() {
        *sequence = sequence.wrapping_add(1);
        inner.broadcaster_signal.notify_all();
    }
}

fn wait_for_broadcaster_signal(inner: &RemoteHostInner, timeout: Duration) {
    let Ok(sequence) = inner.broadcaster_signal_lock.lock() else {
        return;
    };
    let _ = inner.broadcaster_signal.wait_timeout(sequence, timeout);
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

pub(crate) struct RemoteHostInner {
    config: RwLock<RemoteHostConfig>,
    config_update_lock: Mutex<()>,
    /// Serializes listener/runtime restarts without holding the config update
    /// lock across worker joins. Native workers may need that config lock while
    /// completing their disconnect cleanup.
    lifecycle_lock: Mutex<()>,
    config_revision: AtomicU64,
    /// Coordinates publication of workspace state with browser snapshot
    /// capture so a revision always describes the state sent with it.
    snapshot_state_lock: Mutex<()>,
    snapshot_revision: AtomicU64,
    runtime_instance_id: String,
    shared_state: RwLock<AppState>,
    runtime_state: RwLock<RuntimeState>,
    port_statuses: RwLock<HashMap<u16, PortStatus>>,
    port_authorities: RwLock<HashMap<u16, RemotePortAuthority>>,
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
    port_forward_authorizer_test_hook: RwLock<Option<Arc<dyn Fn(u16) -> bool + Send + Sync>>>,
    #[cfg(test)]
    lifecycle_lock_acquired_test_hook: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    worker_reaped_test_hook: RwLock<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
    #[cfg(test)]
    native_lifecycle_test_hook: RwLock<Option<Arc<dyn Fn(NativeLifecycleTestEvent) + Send + Sync>>>,
    #[cfg(test)]
    native_worker_registration_test_hook: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Non-blocking admission handle for the web listener's bounded Push
    /// delivery pool. It is absent whenever the listener is stopped.
    web_push_sender: RwLock<Option<web::push::PushSender>>,
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
    native_runtime_generation: AtomicU64,
    stop_flag: AtomicBool,
    worker_residue_count: AtomicUsize,
    listener_thread: Mutex<Option<RemoteWorker>>,
    broadcaster_thread: Mutex<Option<RemoteWorker>>,
    listener_leases: Mutex<HashMap<u16, u64>>,
    native_listener_wakeup: Mutex<Option<SocketAddr>>,
    broadcaster_signal: Condvar,
    broadcaster_signal_lock: Mutex<u64>,
    native_connection_workers: Mutex<HashMap<u64, NativeConnectionWorker>>,
    // Both fields are written on lifecycle transitions and (Phase 1b+)
    // surfaced through the settings panel; suppress the transient warning.
    #[allow(dead_code)]
    web_listener: Mutex<Option<WebListenerHandle>>,
    #[allow(dead_code)]
    web_listener_error: RwLock<Option<String>>,
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
    sender: Option<mpsc::Sender<ServerMessage>>,
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
    Native(mpsc::Sender<ServerMessage>),
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
        ClientDeliveryTarget::Native(_) => {
            if let Ok(mut clients) = inner.clients.lock() {
                clients.remove(&connection_id);
            }
        }
    }
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

#[derive(Clone)]
pub struct LocalPortForwardManager {
    inner: Arc<LocalPortForwardManagerInner>,
}

struct LocalPortForwardManagerInner {
    client: RemoteClientHandle,
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
    socket_wakeup: Option<TcpStream>,
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
        let mut config = config;
        config.web.ensure_secrets();
        let _ = transport::ensure_host_tls_material(&mut config);
        let inner = Arc::new(RemoteHostInner {
            config: RwLock::new(config.clone()),
            config_update_lock: Mutex::new(()),
            lifecycle_lock: Mutex::new(()),
            config_revision: AtomicU64::new(1),
            snapshot_state_lock: Mutex::new(()),
            snapshot_revision: AtomicU64::new(1),
            runtime_instance_id: generate_secret("runtime"),
            shared_state: RwLock::new(AppState::default()),
            runtime_state: RwLock::new(RuntimeState::default()),
            port_statuses: RwLock::new(HashMap::new()),
            port_authorities: RwLock::new(HashMap::new()),
            semantic_journals: Mutex::new(SemanticJournalStore::default()),
            semantic_publication_lock: Mutex::new(()),
            semantic_publication_generation: AtomicU64::new(0),
            #[cfg(test)]
            semantic_publication_test_hook: RwLock::new(None),
            semantic_delivery_lock: Mutex::new(()),
            #[cfg(test)]
            semantic_delivery_test_hook: RwLock::new(None),
            #[cfg(test)]
            port_forward_authorizer_test_hook: RwLock::new(None),
            #[cfg(test)]
            lifecycle_lock_acquired_test_hook: RwLock::new(None),
            #[cfg(test)]
            worker_reaped_test_hook: RwLock::new(None),
            #[cfg(test)]
            native_lifecycle_test_hook: RwLock::new(None),
            #[cfg(test)]
            native_worker_registration_test_hook: RwLock::new(None),
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
            native_runtime_generation: AtomicU64::new(1),
            stop_flag: AtomicBool::new(false),
            worker_residue_count: AtomicUsize::new(0),
            listener_thread: Mutex::new(None),
            broadcaster_thread: Mutex::new(None),
            listener_leases: Mutex::new(HashMap::new()),
            native_listener_wakeup: Mutex::new(None),
            broadcaster_signal: Condvar::new(),
            broadcaster_signal_lock: Mutex::new(0),
            native_connection_workers: Mutex::new(HashMap::new()),
            web_listener: Mutex::new(None),
            web_listener_error: RwLock::new(None),
        });
        let service = Self {
            _lifetime_owner: Some(RemoteHostServiceOwner {
                inner: inner.clone(),
            }),
            inner,
        };
        service.apply_config(config);
        service
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
            let Ok(_update_guard) = self.inner.config_update_lock.lock() else {
                return;
            };
            if let Ok(mut slot) = self.inner.config.write() {
                *slot = config;
            }
            self.bump_config_revision();
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
        )?
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
        )?
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
        })?;
        Ok(token)
    }

    pub fn regenerate_web_pairing_token(&self) -> Result<String, String> {
        let token = web::generate_web_pairing_token();
        mutate_host_config(&self.inner, |config| {
            config.web.pairing_token = token.clone();
        })?;
        Ok(token)
    }

    pub fn update_snapshot(
        &self,
        app_state: AppState,
        runtime_state: RuntimeState,
        port_statuses: HashMap<u16, PortStatus>,
    ) {
        self.update_snapshot_parts_with_authorities(
            Some(app_state),
            Some(runtime_state),
            Some(port_statuses),
            Some(HashMap::new()),
        );
    }

    pub fn update_snapshot_parts(
        &self,
        app_state: Option<AppState>,
        runtime_state: Option<RuntimeState>,
        port_statuses: Option<HashMap<u16, PortStatus>>,
    ) {
        let port_authorities = port_statuses.as_ref().map(|_| HashMap::new());
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
            .and_then(|sender| sender.clone());
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
        let _operation = self
            .inner
            .web_control_operation_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let removed = match mutate_host_config(&self.inner, |config| {
            let before = config.web.paired_clients.len();
            config
                .web
                .paired_clients
                .retain(|client| client.client_id != client_id);
            config.web.activity_log.retain(|event| {
                !(event.source == RemoteAccessSource::Browser && event.client_id == client_id)
            });
            config.web.push.remove_client(client_id);
            config.web.paired_clients.len() != before
        }) {
            Ok(removed) => removed,
            Err(_) => return false,
        };

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
        let _operation = self
            .inner
            .web_control_operation_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let removed_client_ids = match mutate_host_config(&self.inner, |config| {
            let removed_ids = config
                .web
                .paired_clients
                .iter()
                .map(|client| client.client_id.clone())
                .collect::<Vec<_>>();
            config.web.paired_clients.clear();
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
            Ok(removed_client_ids) => removed_client_ids,
            Err(_) => return false,
        };
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
        self.inner.config_revision.fetch_add(1, Ordering::Relaxed);
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
            let worker = RemoteWorker::spawn("remote-web-restart", None, move || {
                handle.shutdown();
            });
            settle_remote_worker(&self.inner, worker, deadline);
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

        let (native_lease, web_lease) =
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
            };

        let mut new_listener_worker = config.enabled.then(|| {
            let listener_inner = self.inner.clone();
            RemoteWorker::spawn("remote-native-listener", None, move || {
                run_listener(listener_inner, generation, native_lease);
            })
        });
        let mut new_broadcaster_worker = (config.enabled || config.web.enabled).then(|| {
            let broadcaster_inner = self.inner.clone();
            RemoteWorker::spawn("remote-broadcaster", None, move || {
                run_broadcaster(broadcaster_inner, generation);
            })
        });
        let installed = {
            let _lifecycle_guard = self
                .inner
                .lifecycle_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.inner.stop_flag.load(Ordering::Acquire)
                || self.inner.native_runtime_generation.load(Ordering::Acquire) != generation
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
                            *self
                                .inner
                                .web_listener
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                stale_handle.take();
                        }
                    }
                    if let Some(handle) = stale_handle {
                        let worker =
                            RemoteWorker::spawn("remote-stale-web-listener", None, move || {
                                handle.shutdown()
                            });
                        settle_remote_worker(
                            &self.inner,
                            worker,
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
        let reader = RemoteWorker::spawn("remote-client-reader", None, move || {
            run_client_connection(stream, rx, reader_inner)
        });
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
        let transport::TlsConnectResult {
            mut stream,
            certificate_fingerprint,
            handshake_deadline,
        } = transport::connect_tls(
            &self.inner.address,
            self.inner.port,
            Some(&self.inner.certificate_fingerprint),
        )?;
        if certificate_fingerprint != self.inner.certificate_fingerprint {
            return Err(
                "Remote TLS fingerprint changed while opening the forwarded port.".to_string(),
            );
        }
        let _ = stream.sock.set_write_timeout(Some(
            handshake_deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(5)),
        ));
        write_message_until_deadline(
            &mut stream,
            &ClientMessage::PortForwardHello {
                protocol_version: PROTOCOL_VERSION,
                server_id: self.inner.server_id.clone(),
                client_id: self.inner.client_id.clone(),
                auth_token: self.inner.client_token.clone(),
                requested_port,
            },
            handshake_deadline,
        )
        .map_err(|error| format!("Port forward handshake failed: {error}"))?;
        match read_message_until_deadline::<ServerMessage, _>(&mut stream, handshake_deadline)
            .map_err(|error| format!("Port forward handshake failed: {error}"))?
        {
            ServerMessage::PortForwardOk => {
                let _ = stream.sock.set_read_timeout(Some(Duration::from_secs(5)));
                Ok(stream)
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
                    if let Ok(mut entries) = self.inner.entries.lock() {
                        entries.insert(port, entry);
                    }
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
                    if let Ok(mut entries) = self.inner.entries.lock() {
                        entries.insert(
                            port,
                            LocalPortForwardEntry {
                                scope_id: None,
                                stop: None,
                                worker: None,
                                wakeup: None,
                                retry_after_epoch_ms: now_epoch_ms.saturating_add(1000),
                            },
                        );
                    }
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

        let deadline = Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT;
        for (_, mut entry) in entries {
            if let Some(wakeup) = entry.wakeup.take() {
                let _ = TcpStream::connect_timeout(&wakeup, Duration::from_millis(100));
            }
            if let Some(worker) = entry.worker.take() {
                settle_unowned_remote_worker(worker, deadline);
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
        for (_, connection) in connections {
            if let Some(socket) = connection.socket_wakeup.as_ref() {
                let _ = socket.shutdown(Shutdown::Both);
            }
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
        if let Some(socket) = connection.socket_wakeup.as_ref() {
            let _ = socket.shutdown(Shutdown::Both);
        }
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
        handle,
        owner: DeferredRemoteWorkerOwner::LocalPortForward {
            inner: Arc::downgrade(inner),
            port,
        },
    });
}

fn defer_unowned_remote_worker(mut worker: RemoteWorker) {
    let Some(handle) = worker.handle.take() else {
        return;
    };
    enqueue_deferred_remote_worker(DeferredRemoteWorker {
        name: worker.name,
        handle,
        owner: DeferredRemoteWorkerOwner::Unowned,
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

fn settle_unowned_remote_worker(mut worker: RemoteWorker, deadline: Instant) {
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
                    .is_some_and(thread::JoinHandle::is_finished)
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
        if let Some(socket) = connection.socket_wakeup.as_ref() {
            let _ = socket.shutdown(Shutdown::Both);
        }
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
    let worker = RemoteWorker::spawn(format!("local-forward-listener-{port}"), None, move || {
        run_local_port_forward_listener(thread_inner, port, scope_id, listener, stop_flag)
    });
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
                let socket_wakeup = socket.try_clone().ok();
                let connection_id = strong_inner
                    .next_connection_id
                    .fetch_add(1, Ordering::Relaxed);
                let worker = RemoteWorker::spawn(
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
                        )
                    },
                );
                registry.connections.insert(
                    connection_id,
                    LocalPortForwardConnectionWorker {
                        port,
                        scope_id,
                        socket_wakeup,
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
) {
    let _ = local_socket.set_nodelay(true);
    let _ = local_socket.set_read_timeout(None);
    let _ = local_socket.set_write_timeout(None);
    let mut remote_stream = match client.open_port_forward(port) {
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

    if let Err(error) = copy_bidirectional(&mut local_socket, &mut remote_stream, || {
        stop_flag.load(Ordering::Acquire)
    }) {
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

fn wait_for_forward_readability<L: RemoteForwardStream, R: RemoteForwardStream>(
    left: &L,
    right: &R,
) -> std::io::Result<()> {
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
                events: 0x0001,
                revents: 0,
            },
            PollFd {
                fd: right.raw_forward_socket(),
                events: 0x0001,
                revents: 0,
            },
        ];
        let result = unsafe { poll(fds.as_mut_ptr(), fds.len(), -1) };
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
                events: 0x0300,
                revents: 0,
            },
            WsapollFd {
                fd: right.raw_forward_socket(),
                events: 0x0300,
                revents: 0,
            },
        ];
        let result = unsafe { WSAPoll(fds.as_mut_ptr(), fds.len() as u32, -1) };
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
    mut should_stop: impl FnMut() -> bool,
) -> Result<(), String> {
    let mut left_buf = [0_u8; 16 * 1024];
    let mut right_buf = [0_u8; 16 * 1024];
    left.set_forward_nonblocking(true)
        .map_err(|error| format!("Could not configure forward read readiness: {error}"))?;
    right
        .set_forward_nonblocking(true)
        .map_err(|error| format!("Could not configure forward read readiness: {error}"))?;
    loop {
        if should_stop() {
            break;
        }
        let mut made_progress = false;
        match left.read(&mut left_buf) {
            Ok(0) => break,
            Ok(read) => {
                if should_stop() {
                    break;
                }
                left.set_forward_nonblocking(false).map_err(|error| {
                    format!("Could not configure forward write readiness: {error}")
                })?;
                right.set_forward_nonblocking(false).map_err(|error| {
                    format!("Could not configure forward write readiness: {error}")
                })?;
                right
                    .write_all(&left_buf[..read])
                    .map_err(|error| format!("Write failed: {error}"))?;
                right
                    .flush()
                    .map_err(|error| format!("Flush failed: {error}"))?;
                left.set_forward_nonblocking(true).map_err(|error| {
                    format!("Could not restore forward read readiness: {error}")
                })?;
                right.set_forward_nonblocking(true).map_err(|error| {
                    format!("Could not restore forward read readiness: {error}")
                })?;
                made_progress = true;
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => return Err(format!("Read failed: {error}")),
        }

        if should_stop() {
            break;
        }
        match right.read(&mut right_buf) {
            Ok(0) => break,
            Ok(read) => {
                if should_stop() {
                    break;
                }
                left.set_forward_nonblocking(false).map_err(|error| {
                    format!("Could not configure forward write readiness: {error}")
                })?;
                right.set_forward_nonblocking(false).map_err(|error| {
                    format!("Could not configure forward write readiness: {error}")
                })?;
                left.write_all(&right_buf[..read])
                    .map_err(|error| format!("Write failed: {error}"))?;
                left.flush()
                    .map_err(|error| format!("Flush failed: {error}"))?;
                left.set_forward_nonblocking(true).map_err(|error| {
                    format!("Could not restore forward read readiness: {error}")
                })?;
                right.set_forward_nonblocking(true).map_err(|error| {
                    format!("Could not restore forward read readiness: {error}")
                })?;
                made_progress = true;
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => return Err(format!("Read failed: {error}")),
        }
        if !made_progress {
            wait_for_forward_readability(left, right)
                .map_err(|error| format!("Forward readiness wait failed: {error}"))?;
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

fn cancel_native_connection_socket(socket: &TcpStream) {
    // Keep the wake operation on the shared socket so rustls/read-message
    // workers leave their blocking read without changing the live stream's
    // mode or normal handshake deadlines.
    let _ = socket.shutdown(Shutdown::Both);
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

fn spawn_native_connection_worker(
    inner: &Arc<RemoteHostInner>,
    connection_id: u64,
    stream: TcpStream,
    native_runtime_generation: u64,
) {
    // Keep a cancellation socket on the local path until registration has
    // linearized. A restart can race this worker between spawn and the
    // lifecycle-registry lock; in that window the owner has no registry entry
    // from which to wake a blocking TLS read unless this local clone is used.
    let mut socket_wakeup = stream.try_clone().ok();
    let done = Arc::new(AtomicBool::new(false));
    // The stalled TLS phase must not keep the host runtime alive after the
    // owner has revoked the generation. Upgrade this weak reference only for
    // the post-handshake work that needs the live service.
    let thread_inner = Arc::downgrade(inner);
    let worker = RemoteWorker::spawn(
        format!("remote-native-{connection_id}"),
        Some(done.clone()),
        move || {
            handle_client_connection_with_weak(
                thread_inner,
                connection_id,
                stream,
                native_runtime_generation,
            );
        },
    );
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
                        socket_wakeup: socket_wakeup.take(),
                        runtime_hold: Some(inner.clone()),
                        worker: worker.take().expect("native worker should register once"),
                    },
                );
        }
    }
    if let Some(socket) = socket_wakeup {
        cancel_native_connection_socket(&socket);
    }
    if let Some(worker) = worker {
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
        drop(worker.runtime_hold);
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
        if let Some(socket) = worker.socket_wakeup.as_ref() {
            cancel_native_connection_socket(socket);
        }
        drop(worker.runtime_hold);
        settle_remote_worker(inner, worker.worker, deadline);
    }
}

fn run_listener(
    inner: Arc<RemoteHostInner>,
    native_runtime_generation: u64,
    lease: Option<ListenerLease>,
) {
    let Some(lease) = lease else {
        return;
    };
    if !lease.is_current() {
        return;
    }
    let config = inner
        .config
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let bind = format!("{}:{}", config.bind_address, config.port);
    let listener = match TcpListener::bind(&bind) {
        Ok(listener) => listener,
        Err(error) => {
            let failure = ListenerBindFailure::from_io(bind.clone(), error);
            inner.listener_running.store(false, Ordering::Relaxed);
            if let Ok(mut slot) = inner.listener_error.write() {
                *slot = Some(failure.to_string());
            }
            set_last_connection_note(
                &inner,
                format!("Remote host could not start listening: {failure}"),
                true,
            );
            eprintln!("[remote] failed to bind {failure}");
            #[cfg(test)]
            notify_native_lifecycle(&inner, NativeLifecycleTestEvent::ListenerBindFailed);
            return;
        }
    };
    if !lease.is_current() {
        let failure = ListenerBindFailure::GenerationStale {
            bind: bind.clone(),
            phase: "after",
        };
        if let Ok(mut slot) = inner.listener_error.write() {
            *slot = Some(failure.to_string());
        }
        let _ = listener.set_nonblocking(false);
        return;
    }
    inner.listener_running.store(true, Ordering::Relaxed);
    if let Ok(mut slot) = inner.listener_error.write() {
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
        if let Ok(mut slot) = inner.native_listener_wakeup.lock() {
            *slot = Some(wake_addr);
        }
    }
    #[cfg(test)]
    notify_native_lifecycle(&inner, NativeLifecycleTestEvent::ListenerStarted);

    while !native_connection_should_stop(&inner, native_runtime_generation) {
        reap_completed_native_connection_workers(&inner);
        match listener.accept() {
            Ok((stream, _)) => {
                if native_connection_should_stop(&inner, native_runtime_generation) {
                    let _ = stream.shutdown(Shutdown::Both);
                    break;
                }
                let connection_id = inner.next_connection_id.fetch_add(1, Ordering::Relaxed);
                spawn_native_connection_worker(
                    &inner,
                    connection_id,
                    stream,
                    native_runtime_generation,
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                if !native_connection_should_stop(&inner, native_runtime_generation) {
                    if let Ok(mut slot) = inner.listener_error.write() {
                        *slot = Some(format!("Remote listener accept failed: {error}"));
                    }
                }
                break;
            }
        }
    }
    if let Ok(mut slot) = inner.native_listener_wakeup.lock() {
        *slot = None;
    }
    inner.listener_running.store(false, Ordering::Relaxed);
}

fn run_broadcaster(inner: Arc<RemoteHostInner>, native_runtime_generation: u64) {
    let mut last_snapshot_revision = 0_u64;
    let mut last_semantic_delivery_revision = 0_u64;
    let mut last_controller_client_id: Option<String> = None;
    let mut last_bootstrap_retry_at: HashMap<String, Instant> = HashMap::new();

    while !native_connection_should_stop(&inner, native_runtime_generation) {
        reap_completed_native_connection_workers(&inner);
        let connected_clients = inner
            .clients
            .lock()
            .map(|clients| clients.len())
            .unwrap_or(0);
        if connected_clients == 0 {
            wait_for_broadcaster_signal(&inner, IDLE_BROADCAST_INTERVAL);
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
            wait_for_broadcaster_signal(&inner, SNAPSHOT_BROADCAST_INTERVAL);
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
            wait_for_broadcaster_signal(&inner, SNAPSHOT_BROADCAST_INTERVAL);
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

        wait_for_broadcaster_signal(&inner, SNAPSHOT_BROADCAST_INTERVAL);
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
    let worker = RemoteWorker::spawn(worker_name, None, move || {
        let result = permit.run(|| provider(&callback_session_id));
        let _ = result_tx.try_send(result);
    });

    match result_rx.recv_timeout(REMOTE_CALLBACK_TIMEOUT) {
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
        Err(mpsc::RecvTimeoutError::Timeout) => {
            settle_remote_worker(inner, worker, Instant::now());
            None
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            settle_remote_worker(
                inner,
                worker,
                Instant::now() + REMOTE_WORKER_SHUTDOWN_TIMEOUT,
            );
            None
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
    let worker = RemoteWorker::spawn(name, None, move || {
        let result = permit.run(callback);
        let _ = result_tx.try_send(result);
    });
    match result_rx.recv_timeout(REMOTE_CALLBACK_TIMEOUT) {
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
        Err(mpsc::RecvTimeoutError::Timeout) => {
            set_last_connection_note(
                inner,
                "Remote callback exceeded its bounded deadline; worker remains owned for cooperative shutdown."
                    .to_string(),
                true,
            );
            settle_remote_worker(inner, worker, Instant::now());
            None
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
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
    handle_client_connection_with_weak(
        Arc::downgrade(&inner),
        connection_id,
        stream,
        native_runtime_generation,
    );
}

fn handle_client_connection_with_weak(
    inner: Weak<RemoteHostInner>,
    connection_id: u64,
    stream: TcpStream,
    native_runtime_generation: u64,
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

    let (tx, rx) = mpsc::channel::<ServerMessage>();

    let (client_id, client_token, _client_label) = match authenticate_client_and_record_activity(
        &inner,
        hello,
        peer_ip.clone(),
    ) {
        Ok(auth) => auth,
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
    if native_connection_should_stop(&inner, native_runtime_generation) {
        return;
    }

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
    let _registered = if let Ok(mut clients) = inner.clients.lock() {
        clients.insert(
            connection_id,
            ConnectedRemoteClient {
                client_id: client_id.clone(),
                sender: Some(tx.clone()),
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
        true
    } else {
        false
    };
    if _registered {
        notify_broadcaster(&inner);
    }
    #[cfg(test)]
    if _registered {
        notify_native_lifecycle(&inner, NativeLifecycleTestEvent::ClientRegistered);
    }

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
        if let Ok(mut clients) = inner.clients.lock() {
            let _removed = clients.remove(&connection_id).is_some();
            drop(clients);
            if _removed {
                notify_broadcaster(&inner);
            }
            #[cfg(test)]
            if _removed {
                notify_native_lifecycle(&inner, NativeLifecycleTestEvent::ClientRemoved);
            }
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
                        Instant::now() + HEARTBEAT_INTERVAL,
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
    let _removed = inner
        .clients
        .lock()
        .map(|mut clients| clients.remove(&connection_id).is_some())
        .unwrap_or(false);
    if _removed {
        notify_broadcaster(&inner);
    }
    if let Ok(mut controller) = inner.controller_client_id.write() {
        if controller.as_deref() == Some(client_id.as_str()) {
            *controller = None;
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
    if !native_connection_should_stop(&inner, native_runtime_generation) {
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

#[cfg(test)]
fn authenticate_client(
    inner: &Arc<RemoteHostInner>,
    hello: ClientMessage,
) -> Result<(String, String, String), String> {
    authenticate_client_with_activity(inner, hello, None)
}

fn authenticate_client_and_record_activity(
    inner: &Arc<RemoteHostInner>,
    hello: ClientMessage,
    ip_address: Option<String>,
) -> Result<(String, String, String), String> {
    authenticate_client_with_activity(inner, hello, Some(ip_address))
}

fn authenticate_client_with_activity(
    inner: &Arc<RemoteHostInner>,
    hello: ClientMessage,
    activity_ip_address: Option<Option<String>>,
) -> Result<(String, String, String), String> {
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

    match auth {
        ClientAuth::PairToken { token } => {
            let client_id = generate_secret("client");
            let client_token = generate_secret("auth");
            mutate_host_config_if(
                inner,
                |config| token.trim() == config.pairing_token.trim(),
                |config| {
                    config.paired_clients.push(PairedRemoteClient {
                        client_id: client_id.clone(),
                        label: client_label.clone(),
                        auth_token: client_token.clone(),
                        last_seen_epoch_ms: Some(now_epoch_ms()),
                    });
                    if record_activity {
                        append_native_connection_activity(
                            config,
                            client_id.clone(),
                            client_label.clone(),
                            activity_ip_address.clone(),
                        );
                    }
                    (client_id, client_token, client_label)
                },
            )?
            .ok_or_else(|| "Pairing token did not match the host.".to_string())
        }
        ClientAuth::ClientToken {
            client_id,
            auth_token,
        } => mutate_host_config_if(
            inner,
            |config| {
                config
                    .paired_clients
                    .iter()
                    .any(|client| client.client_id == client_id && client.auth_token == auth_token)
            },
            |config| {
                let authenticated = {
                    let client = config
                        .paired_clients
                        .iter_mut()
                        .find(|client| {
                            client.client_id == client_id && client.auth_token == auth_token
                        })
                        .expect("serialized native client condition must remain true");
                    client.label = client_label;
                    client.last_seen_epoch_ms = Some(now_epoch_ms());
                    (
                        client.client_id.clone(),
                        client.auth_token.clone(),
                        client.label.clone(),
                    )
                };
                if record_activity {
                    append_native_connection_activity(
                        config,
                        authenticated.0.clone(),
                        authenticated.2.clone(),
                        activity_ip_address.clone(),
                    );
                }
                authenticated
            },
        )?
        .ok_or_else(|| "Saved remote credentials are no longer valid.".to_string()),
    }
}

fn handle_port_forward_connection(
    inner: &Arc<RemoteHostInner>,
    peer_label: &str,
    stream: &mut transport::ServerTlsStream,
    hello: ClientMessage,
    native_runtime_generation: u64,
    handshake_deadline: Instant,
) -> Result<(), String> {
    let (client_id, auth_token, requested_port) = authenticate_port_forward(inner, hello)?;
    let mut last_connect_error = None;
    let mut upstream = None;
    for address in [
        SocketAddr::from((Ipv4Addr::LOCALHOST, requested_port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, requested_port)),
    ] {
        if native_connection_should_stop(inner, native_runtime_generation) {
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
    }
    let mut upstream = upstream.ok_or_else(|| {
        format!(
            "Could not connect to host localhost:{requested_port}: {}",
            last_connect_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "no loopback address was available".to_string())
        )
    })?;
    let _ = upstream.set_nodelay(true);
    let _ = upstream.set_read_timeout(None);
    let _ = upstream.set_write_timeout(None);
    set_server_handshake_write_deadline(stream, handshake_deadline)?;
    write_message_until_deadline(stream, &ServerMessage::PortForwardOk, handshake_deadline)
        .map_err(|error| format!("Could not start port forward: {error}"))?;
    if let Err(error) = copy_bidirectional(&mut upstream, stream, || {
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
    #[cfg(test)]
    if let Some(authorize) = inner
        .port_forward_authorizer_test_hook
        .read()
        .ok()
        .and_then(|slot| slot.clone())
    {
        return authorize(requested_port);
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
    let now_epoch_ms = now_epoch_ms();
    let port_authorities = inner
        .port_authorities
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();

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
                    && remote_authority_allows_forward(
                        authority,
                        requested_port,
                        session,
                        now_epoch_ms,
                    )
                {
                    return true;
                }
            }
        }
    }
    false
}

fn remote_authority_allows_forward(
    authority: &RemotePortAuthority,
    requested_port: u16,
    session: &SessionRuntimeState,
    now_epoch_ms: u64,
) -> bool {
    authority.is_fresh_at(now_epoch_ms)
        && authority.has_exact_managed_fence_for(requested_port, session)
}

fn bump_host_config_revision(inner: &Arc<RemoteHostInner>) {
    inner.config_revision.fetch_add(1, Ordering::Relaxed);
}

fn set_last_connection_note(inner: &Arc<RemoteHostInner>, note: String, is_error: bool) {
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

fn runtime_owns_port(session: &SessionRuntimeState, status: &PortStatus) -> bool {
    let _ = (session, status);
    // `PortStatus` is the legacy wire shape: it carries only a PID and an
    // optional display name. PID reuse, creation time, executable identity,
    // resource fence, membership revision, and freshness are all absent, so
    // it can never authorize a remote forward or control operation. An exact
    // authority projection must be added to the remote snapshot before this
    // predicate can return true.
    false
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
    let _ = stream.sock.set_read_timeout(Some(HEARTBEAT_INTERVAL));

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
    let port_authorities = inner
        .port_authorities
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
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
    base_snapshot_without_session_views(inner, client_id)
}

#[cfg(test)]
pub(crate) fn current_snapshot(
    inner: &Arc<RemoteHostInner>,
    client_id: &str,
) -> RemoteWorkspaceSnapshot {
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
            let lock = TEST_PROFILE_LOCK.lock().expect("profile lock");
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
        apply_remote_session_output, apply_workspace_delta, authenticate_client,
        current_controller_allows, current_snapshot, deliver_live_semantic_events,
        deliver_pending_bootstraps, drain_web_clients_for_restart, format_handshake_stage_error,
        generate_pairing_token, handle_client_connection, light_snapshot,
        load_remote_machine_state, native_connection_should_stop, now_epoch_ms,
        publish_semantic_event, read_message, read_message_until_cancelled, remote_state_path,
        request_timeout_for_action, requires_control, run_broadcaster, save_remote_known_hosts,
        save_remote_machine_state, set_last_connection_note, spawn_native_connection_worker,
        try_enqueue_pending_request, upsert_known_host, write_message, ClientAuth, ClientMessage,
        ConnectedRemoteClient, HostConfigPersistenceTestPhase, KnownRemoteHost,
        LocalPortForwardLifecycleTestEvent, LocalPortForwardManager, PairedRemoteClient,
        PairedWebClient, PendingRemoteRequest, RemoteAccessActivityEvent, RemoteAccessActivityKind,
        RemoteAccessSource, RemoteAction, RemoteClientHandle, RemoteClientInner, RemoteHostConfig,
        RemoteHostService, RemoteHostWorkLimiter, RemoteLatencyStats, RemoteMachineState,
        RemotePortAuthority, RemoteSessionBootstrap, RemoteSessionStreamEvent,
        RemoteStatePersistenceIoTestPhase, RemoteTerminalInput, RemoteWorker, RemoteWorkspaceDelta,
        RemoteWorkspaceSnapshot, ServerMessage, HOST_CONFIG_PERSISTENCE_TEST_HOOK,
        MAX_PENDING_REMOTE_REQUESTS, REMOTE_STATE_PERMISSION_VERIFY_TEST_HOOK,
        REMOTE_STATE_PERSISTENCE_IO_TEST_HOOK,
    };
    use crate::domain::id::ResourceId;
    use crate::domain::operation::ResourceFence;
    use crate::models::{PortStatus, SessionTab, TabType};
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
    use std::collections::{HashMap, HashSet};
    use std::io::{ErrorKind, Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex, RwLock};
    use std::thread;
    use std::time::{Duration, Instant};

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
        let config = RemoteHostConfig {
            enabled: true,
            bind_address: "127.0.0.1".to_string(),
            port,
            ..RemoteHostConfig::default()
        };
        let root = RemoteHostService::new(config);
        wait_for(
            || root.status().listening,
            Duration::from_secs(3),
            "native listener never started",
        );
        let baseline_references = Arc::strong_count(&root.inner);
        let stalled_client =
            TcpStream::connect(("127.0.0.1", port)).expect("stalled native client should connect");
        wait_for(
            || Arc::strong_count(&root.inner) > baseline_references,
            Duration::from_secs(3),
            "native listener never admitted the stalled TLS worker",
        );
        let inner = Arc::downgrade(&root.inner);

        drop(root);

        wait_for(
            || inner.upgrade().is_none(),
            Duration::from_secs(2),
            "stalled native TLS worker retained the stopped host runtime",
        );
        drop(stalled_client);
    }

    #[test]
    fn dropping_root_service_releases_a_tls_client_that_withholds_hello() {
        let port = reserve_free_tcp_port();
        let config = RemoteHostConfig {
            enabled: true,
            bind_address: "127.0.0.1".to_string(),
            port,
            ..RemoteHostConfig::default()
        };
        let root = RemoteHostService::new(config);
        wait_for(
            || root.status().listening,
            Duration::from_secs(3),
            "native listener never started",
        );
        let stalled_client = super::transport::connect_tls("127.0.0.1", port, None)
            .expect("TLS-only native client should complete transport handshake")
            .stream;
        let inner = Arc::downgrade(&root.inner);

        drop(root);

        wait_for(
            || inner.upgrade().is_none(),
            Duration::from_secs(2),
            "TLS client that withheld hello retained the stopped host runtime",
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
        assert!(error.contains("permission"));
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
            assert!(error.contains(detail), "unexpected error: {error}");
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
        assert!(error.contains("parent sync"), "unexpected error: {error}");
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
    fn production_remote_loops_have_no_short_timeout_polling() {
        let source = include_str!("mod.rs");
        let production = source
            .split("#[cfg(test)]\nmod tests {")
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
                    sender: Some(native_tx),
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
                    sender: Some(native_tx),
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
                    sender: Some(subscribed_tx),
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
                    sender: Some(idle_tx),
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
    fn semantic_output_is_recorded_without_raw_terminal_subscribers() {
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
        assert_eq!(output, vec!["red\nnext"]);
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
    fn ai_push_session_output_projects_screen_snapshot_instead_of_byte_dumps() {
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
        assert_eq!(outputs, vec!["frame two"]);
        assert!(replay.events.iter().any(|event| {
            matches!(event.kind, SemanticEventKind::Output { .. })
                && event.replaces_sequence.is_some()
        }));
    }

    #[test]
    fn semantic_projection_runs_outside_the_snapshot_state_lock() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let mut app = AppState::default();
        app.open_tabs.push(SessionTab {
            id: "tab-stable".to_string(),
            tab_type: TabType::Claude,
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
                    .semantic_replay(&StableSessionKey::from_tab("tab-stable"), 0)
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
        *service.inner.web_push_sender.write().unwrap() = Some(PushSender::single(sender));
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
        assert_eq!(delivery.payload.route, "/session/server/server-a");
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
        *service.inner.web_push_sender.write().unwrap() = Some(PushSender::single(sender));

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
                    sender: Some(tx),
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
                    sender: Some(tx),
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
                sender: Some(tx),
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
                    sender: Some(tx),
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

        let broadcaster_inner = service.inner.clone();
        let generation = service
            .inner
            .native_runtime_generation
            .load(Ordering::Acquire);
        let broadcaster = RemoteWorker::spawn("test-reentrant-broadcaster", None, move || {
            run_broadcaster(broadcaster_inner, generation);
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
            .expect("reaper hook lock") = Some(Arc::new(move |name| {
            worker_reaped_tx
                .send(name.to_string())
                .expect("reaper observer should remain");
        }));
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
        assert_eq!(reaped_worker, "test-blocked-broadcaster");
        assert_eq!(
            observer.inner.worker_residue_count.load(Ordering::Acquire),
            0,
            "joined deferred worker remained reported as residue"
        );
    }

    #[test]
    fn deferred_worker_reaper_joins_completed_work_behind_a_blocked_worker() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (reaped_tx, reaped_rx) = mpsc::channel();
        *service
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("reaper hook lock") = Some(Arc::new(move |name| {
            reaped_tx
                .send(name.to_string())
                .expect("reaper observer should remain");
        }));

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
            first_reaped, "test-reaper-completed",
            "a blocked deferred worker prevented the reaper from joining independent completed work"
        );
        blocked_release_tx
            .send(())
            .expect("blocked worker should still be waiting");
        let second_reaped = reaped_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("released deferred worker should eventually be joined");
        assert_eq!(
            second_reaped, "test-reaper-blocked",
            "the released deferred worker should be reaped after its completion"
        );
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
        service.update_snapshot_parts(None, Some(next_runtime.clone()), None);

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
    fn legacy_pid_only_port_status_cannot_prove_remote_forward_authority() {
        let mut session = SessionRuntimeState::new(
            "remote-port-authority",
            PathBuf::new(),
            SessionDimensions::default(),
            TerminalBackend::default(),
        );
        session.status = SessionStatus::Running;
        session.pid = Some(4242);
        let status = PortStatus {
            port: 43123,
            in_use: true,
            pid: Some(4242),
            process_name: Some("node".to_string()),
        };

        assert!(!super::runtime_owns_port(&session, &status));
    }

    #[test]
    fn update_snapshot_parts_ignores_empty_updates() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let before_revision = service.inner.snapshot_revision.load(Ordering::Relaxed);

        service.update_snapshot_parts(None, None, None);

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

        service.push_session_output("alpha", b"hello\r\n".to_vec());
        wait_for(
            || {
                result
                    .client
                    .session_screen_text("alpha")
                    .is_some_and(|text| text.contains("hello"))
            },
            Duration::from_secs(3),
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
        let premature_reply = match hello_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(reply) => Some(reply),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("handshake reply worker disappeared")
            }
        };
        let acknowledged_before_settlement = premature_reply.is_some();
        persistence_release_tx
            .send(())
            .expect("activity persistence should still be waiting");
        persistence_settled_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("isolated activity file should finish its durable write");
        let reply = match premature_reply {
            Some(reply) => reply,
            None => hello_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("host should settle the handshake after persistence"),
        }
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

        assert!(
            !acknowledged_before_settlement,
            "host acknowledged HelloOk before its activity write was durable"
        );
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
            managed_server_runtime("command-web", 4242),
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
            managed_server_runtime("command-web", 4242),
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
            managed_server_runtime("command-web", 4242),
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
        let authority = RemotePortAuthority::from_rich(&live_status, now_epoch_ms())
            .with_snapshot_metadata(live_snapshot.publication_sequence(), 1, 1);
        let runtime = managed_server_runtime("command-web", process_id);
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
        *root
            .inner
            .port_forward_authorizer_test_hook
            .write()
            .expect("port-forward test hook lock") =
            Some(Arc::new(move |port| port == server_port));

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
            let mut after_shutdown = [0_u8; 1];
            let closed = matches!(stream.read(&mut after_shutdown), Ok(0) | Err(_));
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
            let mut after_shutdown = [0_u8; 1];
            let closed = matches!(stream.read(&mut after_shutdown), Ok(0) | Err(_));
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
        let (worker_reaped_tx, worker_reaped_rx) = mpsc::channel();
        *root
            .inner
            .worker_reaped_test_hook
            .write()
            .expect("worker reaped hook lock") = Some(Arc::new(move |name| {
            worker_reaped_tx
                .send(name.to_string())
                .expect("worker reaped observer should remain");
        }));

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
        assert_eq!(
            worker_reaped_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("deferred native listener should be reaped"),
            "remote-native-listener"
        );
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
        let broadcaster_inner = service.inner.clone();
        let generation = service
            .inner
            .native_runtime_generation
            .load(Ordering::Acquire);
        let broadcaster = thread::spawn(move || run_broadcaster(broadcaster_inner, generation));
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
        let sender = web_sender.is_none().then_some(sender);
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

    fn managed_server_runtime(command_id: &str, pid: u32) -> RuntimeState {
        let mut runtime = RuntimeState::default();
        let mut session = SessionRuntimeState::new(
            command_id.to_string(),
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.status = crate::state::SessionStatus::Running;
        session.pid = Some(pid);
        session.command_id = Some(command_id.to_string());
        session.resources.process_ids.push(pid);
        runtime.sessions.insert(command_id.to_string(), session);
        runtime
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
