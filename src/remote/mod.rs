mod client_pool;
mod transport;
pub mod web;

pub use client_pool::RemoteClientPool;
pub use web::{PairedWebClient, WebConfig, WebListenerHandle};

use crate::git::git_service::{
    AiCommitMessage, DeviceCodeResponse, GitBranch, GitDiffResult, GitLogEntry, GitStatusResult,
};
use crate::models::{
    PortStatus, Project, ProjectFolder, RootScanEntry, RunCommand, SSHConnection, ScanResult,
    Settings, TabType,
};
use crate::persistence::{self, PersistenceError};
use crate::state::{AppState, RuntimeState, SessionDimensions, SessionRuntimeState};
use crate::terminal::session::{
    TerminalReplica, TerminalScreenSnapshot, TerminalSearchMatch, TerminalSessionView,
};
use rmp_serde::{decode::from_slice as from_messagepack_slice, encode::to_vec_named};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const PROTOCOL_VERSION: u32 = 5;
const REMOTE_FILE_NAME: &str = "remote.json";
const SNAPSHOT_BROADCAST_INTERVAL: Duration = Duration::from_millis(33);
const IDLE_BROADCAST_INTERVAL: Duration = Duration::from_millis(250);
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

type SessionBootstrapProvider = Arc<dyn Fn(&str) -> Option<RemoteSessionBootstrap> + Send + Sync>;
type TerminalInputHandler =
    Arc<dyn Fn(RemoteTerminalInput, u64) -> Result<(), String> + Send + Sync>;
type TerminalResizeHandler = Arc<dyn Fn(String, SessionDimensions) + Send + Sync>;
type FocusedSessionHandler = Arc<dyn Fn(String) + Send + Sync>;

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
    pub controller_client_id: Option<String>,
    pub you_have_control: bool,
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
    Paste {
        session_id: String,
        text: String,
    },
    Image {
        session_id: String,
        attachment: RemoteImageAttachment,
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

pub fn load_remote_machine_state() -> Result<RemoteMachineState, PersistenceError> {
    let path = remote_state_path()?;
    if !path.exists() {
        return Ok(RemoteMachineState::default());
    }
    let contents = fs::read_to_string(&path).map_err(|source| PersistenceError::Io {
        path: path.clone(),
        source,
    })?;
    serde_json::from_str(&contents).map_err(|source| PersistenceError::Parse { path, source })
}

pub fn save_remote_machine_state(state: &RemoteMachineState) -> Result<(), PersistenceError> {
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
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, json).map_err(|source| PersistenceError::Io {
        path: temp_path.clone(),
        source,
    })?;
    fs::rename(&temp_path, &path).map_err(|source| PersistenceError::Io { path, source })?;
    Ok(())
}

fn persist_host_config_snapshot(config: &RemoteHostConfig) -> Result<(), PersistenceError> {
    let mut state = load_remote_machine_state()?;
    state.host = config.clone();
    save_remote_machine_state(&state)
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

pub fn remote_state_path() -> Result<PathBuf, PersistenceError> {
    Ok(persistence::app_config_dir()?.join(REMOTE_FILE_NAME))
}

pub fn generate_pairing_token() -> String {
    generate_secret("pair").chars().rev().take(6).collect()
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
    let millis = now_epoch_ms();
    format!("{prefix}-{millis:x}-{:x}", std::process::id())
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

#[derive(Clone)]
pub struct RemoteHostService {
    inner: Arc<RemoteHostInner>,
}

pub(crate) struct RemoteHostInner {
    config: RwLock<RemoteHostConfig>,
    config_update_lock: Mutex<()>,
    config_revision: AtomicU64,
    snapshot_revision: AtomicU64,
    shared_state: RwLock<AppState>,
    runtime_state: RwLock<RuntimeState>,
    port_statuses: RwLock<HashMap<u16, PortStatus>>,
    session_bootstrap_provider: RwLock<Option<SessionBootstrapProvider>>,
    terminal_input_handler: RwLock<Option<TerminalInputHandler>>,
    terminal_resize_handler: RwLock<Option<TerminalResizeHandler>>,
    focused_session_handler: RwLock<Option<FocusedSessionHandler>>,
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
    stop_flag: AtomicBool,
    listener_thread: Mutex<Option<thread::JoinHandle<()>>>,
    broadcaster_thread: Mutex<Option<thread::JoinHandle<()>>>,
    // Both fields are written on lifecycle transitions and (Phase 1b+)
    // surfaced through the settings panel; suppress the transient warning.
    #[allow(dead_code)]
    web_listener: Mutex<Option<WebListenerHandle>>,
    #[allow(dead_code)]
    web_listener_error: RwLock<Option<String>>,
}

#[derive(Clone)]
struct ConnectedRemoteClient {
    client_id: String,
    sender: mpsc::Sender<ServerMessage>,
    subscribed_session_ids: HashSet<String>,
    bootstrapped_session_ids: HashSet<String>,
    focused_session_id: Option<String>,
    last_app_hash: u64,
    last_runtime_hash: u64,
    last_port_hash: u64,
    last_controller_client_id: Option<String>,
    last_you_have_control: bool,
}

#[derive(Clone)]
pub struct RemoteClientHandle {
    inner: Arc<RemoteClientInner>,
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
    outgoing: mpsc::Sender<ClientMessage>,
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
}

#[derive(Clone)]
pub struct LocalPortForwardManager {
    inner: Arc<LocalPortForwardManagerInner>,
}

struct LocalPortForwardManagerInner {
    client: RemoteClientHandle,
    entries: Mutex<HashMap<u16, LocalPortForwardEntry>>,
    statuses: RwLock<HashMap<u16, RemotePortForwardState>>,
}

struct LocalPortForwardEntry {
    stop: Option<Arc<AtomicBool>>,
    handle: Option<thread::JoinHandle<()>>,
    retry_after_epoch_ms: u64,
}

impl RemoteHostService {
    pub fn new(config: RemoteHostConfig) -> Self {
        let mut config = config;
        let _ = transport::ensure_host_tls_material(&mut config);
        let service = Self {
            inner: Arc::new(RemoteHostInner {
                config: RwLock::new(config.clone()),
                config_update_lock: Mutex::new(()),
                config_revision: AtomicU64::new(1),
                snapshot_revision: AtomicU64::new(1),
                shared_state: RwLock::new(AppState::default()),
                runtime_state: RwLock::new(RuntimeState::default()),
                port_statuses: RwLock::new(HashMap::new()),
                session_bootstrap_provider: RwLock::new(None),
                terminal_input_handler: RwLock::new(None),
                terminal_resize_handler: RwLock::new(None),
                focused_session_handler: RwLock::new(None),
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
                stop_flag: AtomicBool::new(false),
                listener_thread: Mutex::new(None),
                broadcaster_thread: Mutex::new(None),
                web_listener: Mutex::new(None),
                web_listener_error: RwLock::new(None),
            }),
        };
        service.apply_config(config);
        service
    }

    pub fn apply_config(&self, config: RemoteHostConfig) {
        let mut config = config;
        let _ = transport::ensure_host_tls_material(&mut config);
        let Ok(_update_guard) = self.inner.config_update_lock.lock() else {
            return;
        };
        if let Ok(mut slot) = self.inner.config.write() {
            *slot = config;
        }
        self.bump_config_revision();
        self.restart_threads();
    }

    pub fn update_snapshot(
        &self,
        app_state: AppState,
        runtime_state: RuntimeState,
        port_statuses: HashMap<u16, PortStatus>,
    ) {
        self.update_snapshot_parts(Some(app_state), Some(runtime_state), Some(port_statuses));
    }

    pub fn update_snapshot_parts(
        &self,
        app_state: Option<AppState>,
        runtime_state: Option<RuntimeState>,
        port_statuses: Option<HashMap<u16, PortStatus>>,
    ) {
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
        if changed {
            self.inner.snapshot_revision.fetch_add(1, Ordering::Relaxed);
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

    fn try_bootstrap_session(&self, session_id: &str) -> Option<RemoteSessionBootstrap> {
        self.inner
            .session_bootstrap_provider
            .read()
            .ok()
            .and_then(|provider| provider.as_ref().cloned())
            .and_then(|provider| provider(session_id))
    }

    fn auto_bootstrap_subscribed_clients(&self, session_id: &str) {
        let needs_bootstrap = {
            let Ok(clients) = self.inner.clients.lock() else {
                return;
            };
            clients.values().any(|client| {
                client.subscribed_session_ids.contains(session_id)
                    && !client.bootstrapped_session_ids.contains(session_id)
            })
        };
        if !needs_bootstrap {
            return;
        }

        let Some(bootstrap) = self.try_bootstrap_session(session_id) else {
            return;
        };

        let Ok(mut clients) = self.inner.clients.lock() else {
            return;
        };
        let mut dead_connections = Vec::new();
        for (connection_id, client) in clients.iter_mut() {
            if !client.subscribed_session_ids.contains(session_id)
                || client.bootstrapped_session_ids.contains(session_id)
            {
                continue;
            }

            if client
                .sender
                .send(ServerMessage::SessionStream {
                    event: RemoteSessionStreamEvent::Bootstrap {
                        bootstrap: bootstrap.clone(),
                    },
                })
                .is_ok()
            {
                client
                    .bootstrapped_session_ids
                    .insert(session_id.to_string());
            } else {
                dead_connections.push(*connection_id);
            }
        }
        for connection_id in dead_connections {
            clients.remove(&connection_id);
        }
    }

    pub fn push_session_output(&self, session_id: &str, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        self.auto_bootstrap_subscribed_clients(session_id);
        let Ok(mut clients) = self.inner.clients.lock() else {
            return;
        };
        let mut dead_connections = Vec::new();
        for (connection_id, client) in clients.iter_mut() {
            if !client.subscribed_session_ids.contains(session_id) {
                continue;
            }
            let message = ServerMessage::SessionStream {
                event: RemoteSessionStreamEvent::Output {
                    session_id: session_id.to_string(),
                    chunk_seq: self
                        .inner
                        .next_output_chunk_seq
                        .fetch_add(1, Ordering::Relaxed),
                    emitted_at_epoch_ms: now_epoch_ms(),
                    bytes: bytes.clone(),
                },
            };
            if client.sender.send(message).is_err() {
                dead_connections.push(*connection_id);
            }
        }
        for connection_id in dead_connections {
            clients.remove(&connection_id);
        }
    }

    pub fn push_session_runtime(&self, session_id: &str, runtime: SessionRuntimeState) {
        self.auto_bootstrap_subscribed_clients(session_id);
        let Ok(mut clients) = self.inner.clients.lock() else {
            return;
        };
        let mut dead_connections = Vec::new();
        for (connection_id, client) in clients.iter_mut() {
            if !client.subscribed_session_ids.contains(session_id) {
                continue;
            }
            if !runtime.status.is_live() {
                client.bootstrapped_session_ids.remove(session_id);
            }
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
            if client
                .sender
                .send(ServerMessage::SessionStream { event })
                .is_err()
            {
                dead_connections.push(*connection_id);
            }
        }
        for connection_id in dead_connections {
            clients.remove(&connection_id);
        }
    }

    pub fn push_session_removed(&self, session_id: &str) {
        let Ok(mut clients) = self.inner.clients.lock() else {
            return;
        };
        let mut dead_connections = Vec::new();
        for (connection_id, client) in clients.iter_mut() {
            if !client.subscribed_session_ids.contains(session_id) {
                continue;
            }
            client.bootstrapped_session_ids.remove(session_id);
            if client
                .sender
                .send(ServerMessage::SessionStream {
                    event: RemoteSessionStreamEvent::Removed {
                        session_id: session_id.to_string(),
                    },
                })
                .is_err()
            {
                dead_connections.push(*connection_id);
            }
        }
        for connection_id in dead_connections {
            clients.remove(&connection_id);
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
        let mut removed = false;
        let Ok(_update_guard) = self.inner.config_update_lock.lock() else {
            return false;
        };
        if let Ok(mut config) = self.inner.config.write() {
            let before = config.paired_clients.len();
            config
                .paired_clients
                .retain(|client| client.client_id != client_id);
            removed = config.paired_clients.len() != before;
        }
        if removed {
            self.bump_config_revision();
        }

        if let Ok(mut clients) = self.inner.clients.lock() {
            let connection_ids: Vec<u64> = clients
                .iter()
                .filter_map(|(connection_id, client)| {
                    (client.client_id == client_id).then_some(*connection_id)
                })
                .collect();
            for connection_id in connection_ids {
                if let Some(client) = clients.remove(&connection_id) {
                    let _ = client.sender.send(ServerMessage::Disconnected {
                        message: "This host revoked the saved client token.".to_string(),
                    });
                }
            }
        }

        if let Ok(mut controller) = self.inner.controller_client_id.write() {
            if controller.as_deref() == Some(client_id) {
                *controller = None;
            }
        }

        removed
    }

    pub fn revoke_paired_web_client(&self, client_id: &str) -> bool {
        let mut removed = false;
        let Ok(_update_guard) = self.inner.config_update_lock.lock() else {
            return false;
        };
        if let Ok(mut config) = self.inner.config.write() {
            let before = config.web.paired_clients.len();
            config
                .web
                .paired_clients
                .retain(|client| client.client_id != client_id);
            removed = config.web.paired_clients.len() != before;
        }
        if removed {
            self.bump_config_revision();
        }

        if let Ok(mut clients) = self.inner.clients.lock() {
            let connection_ids: Vec<u64> = clients
                .iter()
                .filter_map(|(connection_id, client)| {
                    (client.client_id == client_id).then_some(*connection_id)
                })
                .collect();
            for connection_id in connection_ids {
                if let Some(client) = clients.remove(&connection_id) {
                    let _ = client.sender.send(ServerMessage::Disconnected {
                        message: "This browser invite was revoked. Pair again to reconnect."
                            .to_string(),
                    });
                }
            }
        }

        if let Ok(mut controller) = self.inner.controller_client_id.write() {
            if controller.as_deref() == Some(client_id) {
                *controller = None;
            }
        }

        removed
    }

    pub fn local_has_control(&self) -> bool {
        self.inner
            .controller_client_id
            .read()
            .map(|slot| slot.is_none())
            .unwrap_or(true)
    }

    pub fn take_local_control(&self) {
        if let Ok(mut controller) = self.inner.controller_client_id.write() {
            *controller = None;
        }
    }

    fn bump_config_revision(&self) {
        self.inner.config_revision.fetch_add(1, Ordering::Relaxed);
    }

    fn restart_threads(&self) {
        self.inner.stop_flag.store(true, Ordering::SeqCst);
        self.inner.listener_running.store(false, Ordering::Relaxed);
        if let Ok(mut error) = self.inner.listener_error.write() {
            *error = None;
        }
        if let Ok(mut note) = self.inner.last_connection_note.write() {
            *note = None;
        }
        self.inner
            .last_connection_is_error
            .store(false, Ordering::Relaxed);
        if let Ok(mut handle) = self.inner.listener_thread.lock() {
            if let Some(thread) = handle.take() {
                let _ = thread.join();
            }
        }
        if let Ok(mut handle) = self.inner.broadcaster_thread.lock() {
            if let Some(thread) = handle.take() {
                let _ = thread.join();
            }
        }
        // Tear down the web listener independently of the TCP listener so
        // rebinding one does not require tearing down the other.
        if let Ok(mut slot) = self.inner.web_listener.lock() {
            if let Some(handle) = slot.take() {
                handle.shutdown();
            }
        }
        if let Ok(mut error) = self.inner.web_listener_error.write() {
            *error = None;
        }
        self.inner.stop_flag.store(false, Ordering::SeqCst);

        let config = self
            .inner
            .config
            .read()
            .map(|slot| slot.clone())
            .unwrap_or_default();

        if config.enabled {
            let listener_inner = self.inner.clone();
            let listener_thread = thread::spawn(move || run_listener(listener_inner));
            if let Ok(mut handle) = self.inner.listener_thread.lock() {
                *handle = Some(listener_thread);
            }
        }

        // The broadcaster drives snapshot/delta fan-out to every connected
        // client, regardless of transport. Run it whenever any listener is
        // enabled — the native TCP one, the browser web one, or both —
        // otherwise web clients would connect and never see a single delta.
        if config.enabled || config.web.enabled {
            let broadcaster_inner = self.inner.clone();
            let broadcaster_thread = thread::spawn(move || run_broadcaster(broadcaster_inner));
            if let Ok(mut handle) = self.inner.broadcaster_thread.lock() {
                *handle = Some(broadcaster_thread);
            }
        }

        // Web listener runs independently of the native TCP listener: users
        // can enable just the web UI if they only care about browser access,
        // or vice versa.
        if config.web.enabled {
            match WebListenerHandle::start(self.inner.clone(), config.web.clone()) {
                Ok(handle) => {
                    if let Ok(mut slot) = self.inner.web_listener.lock() {
                        *slot = Some(handle);
                    }
                }
                Err(error) => {
                    if let Ok(mut error_slot) = self.inner.web_listener_error.write() {
                        *error_slot = Some(error);
                    }
                }
            }
        }
    }
}

impl Drop for RemoteHostService {
    fn drop(&mut self) {
        self.inner.stop_flag.store(true, Ordering::SeqCst);
    }
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
        } = transport::connect_tls(address, port, expected_fingerprint)?;
        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_label: client_label.to_string(),
            auth,
        };
        write_message(&mut stream, &hello)
            .map_err(|error| format_handshake_stage_error(address, port, "write", &error))?;
        let response: ServerMessage = read_message(&mut stream)
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
            outgoing: tx.clone(),
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
        });

        let reader_inner = inner.clone();
        thread::spawn(move || run_client_connection(stream, rx, reader_inner));
        if !initial_subscriptions.is_empty() {
            let _ = tx.send(ClientMessage::SubscribeSessions {
                session_ids: initial_subscriptions,
            });
        }

        Ok(RemoteClientConnectResult {
            client: Self { inner },
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
            .inner
            .outgoing
            .send(ClientMessage::SetFocusedSession { session_id });
    }

    pub fn subscribe_sessions(&self, session_ids: Vec<String>) {
        if session_ids.is_empty() {
            return;
        }
        let _ = self
            .inner
            .outgoing
            .send(ClientMessage::SubscribeSessions { session_ids });
    }

    pub fn unsubscribe_sessions(&self, session_ids: Vec<String>) {
        if session_ids.is_empty() {
            return;
        }
        let _ = self
            .inner
            .outgoing
            .send(ClientMessage::UnsubscribeSessions { session_ids });
    }

    pub fn send_terminal_input(&self, input: RemoteTerminalInput) {
        let _ = self.inner.outgoing.send(ClientMessage::TerminalInput {
            input,
            enqueued_at_epoch_ms: now_epoch_ms(),
        });
    }

    pub fn send_terminal_resize(&self, session_id: String, dimensions: SessionDimensions) {
        let _ = self.inner.outgoing.send(ClientMessage::ResizeSession {
            session_id,
            dimensions,
        });
    }

    pub fn send_action(&self, action: RemoteAction) {
        let _ = self.inner.outgoing.send(ClientMessage::Action { action });
    }

    pub fn take_control(&self) {
        if let Ok(mut latest) = self.inner.latest_snapshot.write() {
            if let Some(snapshot) = latest.as_mut() {
                snapshot.controller_client_id = Some(self.inner.client_id.clone());
                snapshot.you_have_control = true;
            }
        }
        self.inner.snapshot_revision.fetch_add(1, Ordering::Relaxed);
        let _ = self.inner.outgoing.send(ClientMessage::TakeControl);
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
        let _ = self.inner.outgoing.send(ClientMessage::ReleaseControl);
    }

    pub fn disconnect(&self) {
        let _ = self.inner.outgoing.send(ClientMessage::Disconnect);
    }

    pub fn request(&self, action: RemoteAction) -> Result<RemoteActionResult, String> {
        let request_id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        if let Ok(mut pending) = self.inner.pending.lock() {
            pending.insert(request_id, tx);
        }
        self.inner
            .outgoing
            .send(ClientMessage::Request { request_id, action })
            .map_err(|error| format!("Remote request failed: {error}"))?;
        rx.recv_timeout(REQUEST_TIMEOUT)
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
        write_message(
            &mut stream,
            &ClientMessage::PortForwardHello {
                protocol_version: PROTOCOL_VERSION,
                server_id: self.inner.server_id.clone(),
                client_id: self.inner.client_id.clone(),
                auth_token: self.inner.client_token.clone(),
                requested_port,
            },
        )
        .map_err(|error| format!("Port forward handshake failed: {error}"))?;
        match read_message::<ServerMessage, _>(&mut stream)
            .map_err(|error| format!("Port forward handshake failed: {error}"))?
        {
            ServerMessage::PortForwardOk => Ok(stream),
            ServerMessage::HelloErr { message } => Err(message),
            other => Err(format!("Unexpected port forward response: {other:?}")),
        }
    }

    fn note_output_received(&self, emitted_at_epoch_ms: u64) {
        let now_ms = now_epoch_ms();
        if let Ok(mut latency) = self.inner.latency.write() {
            latency.output_host_to_client_ms = Some(now_ms.saturating_sub(emitted_at_epoch_ms));
        }
        self.inner
            .pending_paint_received_at_epoch_ms
            .store(now_ms, Ordering::Relaxed);
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

impl LocalPortForwardManager {
    pub fn new(client: RemoteClientHandle) -> Self {
        Self {
            inner: Arc::new(LocalPortForwardManagerInner {
                client,
                entries: Mutex::new(HashMap::new()),
                statuses: RwLock::new(HashMap::new()),
            }),
        }
    }

    pub fn sync_ports(&self, desired_ports: &[u16]) -> bool {
        let desired = desired_ports.iter().copied().collect::<HashSet<_>>();
        let now_epoch_ms = now_epoch_ms();
        let mut changed = false;

        let Ok(mut entries) = self.inner.entries.lock() else {
            return false;
        };

        let existing_ports = entries.keys().copied().collect::<Vec<_>>();
        for port in existing_ports {
            if desired.contains(&port) {
                continue;
            }
            if let Some(mut entry) = entries.remove(&port) {
                stop_local_port_forward_entry(&mut entry);
            }
            if let Ok(mut statuses) = self.inner.statuses.write() {
                statuses.remove(&port);
            }
            changed = true;
        }

        for &port in &desired {
            let listener_active = self
                .inner
                .statuses
                .read()
                .ok()
                .and_then(|statuses| statuses.get(&port).map(|state| state.listener_active))
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
            if let Some(mut old_entry) = entries.remove(&port) {
                stop_local_port_forward_entry(&mut old_entry);
            }
            match start_local_port_forward_listener(self.inner.clone(), port) {
                Ok(entry) => {
                    entries.insert(port, entry);
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
                    entries.insert(
                        port,
                        LocalPortForwardEntry {
                            stop: None,
                            handle: None,
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
            changed = true;
        }

        changed
    }

    pub fn shutdown(&self) {
        let Ok(mut entries) = self.inner.entries.lock() else {
            return;
        };
        for entry in entries.values_mut() {
            stop_local_port_forward_entry(entry);
        }
        entries.clear();
        if let Ok(mut statuses) = self.inner.statuses.write() {
            statuses.clear();
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

fn stop_local_port_forward_entry(entry: &mut LocalPortForwardEntry) {
    if let Some(stop) = entry.stop.take() {
        stop.store(true, Ordering::SeqCst);
    }
    if let Some(handle) = entry.handle.take() {
        let _ = handle.join();
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
        .set_nonblocking(true)
        .map_err(|error| format!("Could not configure localhost:{port}: {error}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let thread_inner = inner.clone();
    let handle = thread::spawn(move || {
        run_local_port_forward_listener(thread_inner, port, listener, stop_flag)
    });
    Ok(LocalPortForwardEntry {
        stop: Some(stop),
        handle: Some(handle),
        retry_after_epoch_ms: 0,
    })
}

fn run_local_port_forward_listener(
    inner: Arc<LocalPortForwardManagerInner>,
    port: u16,
    listener: TcpListener,
    stop_flag: Arc<AtomicBool>,
) {
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

    while !stop_flag.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((socket, _)) => {
                let connection_inner = inner.clone();
                let client = inner.client.clone();
                thread::spawn(move || {
                    handle_local_port_forward_connection(connection_inner, client, port, socket)
                });
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(12));
            }
            Err(error) => {
                set_port_forward_state(
                    &inner,
                    RemotePortForwardState {
                        port,
                        listener_active: false,
                        local_port_busy: false,
                        message: Some(format!("Local forward listener on {port} failed: {error}")),
                    },
                );
                return;
            }
        }
    }
}

fn handle_local_port_forward_connection(
    inner: Arc<LocalPortForwardManagerInner>,
    client: RemoteClientHandle,
    port: u16,
    mut local_socket: TcpStream,
) {
    let _ = local_socket.set_nodelay(true);
    let _ = local_socket.set_read_timeout(Some(Duration::from_millis(40)));
    let _ = local_socket.set_write_timeout(Some(Duration::from_secs(5)));
    let mut remote_stream = match client.open_port_forward(port) {
        Ok(stream) => stream,
        Err(error) => {
            set_port_forward_state(
                &inner,
                RemotePortForwardState {
                    port,
                    listener_active: true,
                    local_port_busy: false,
                    message: Some(format!("Tunnel error on localhost:{port}: {error}")),
                },
            );
            let _ = local_socket.shutdown(Shutdown::Both);
            return;
        }
    };

    if let Err(error) = copy_bidirectional(&mut local_socket, &mut remote_stream) {
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
    let _ = remote_stream.sock.shutdown(Shutdown::Both);
}

fn copy_bidirectional<L: Read + Write, R: Read + Write>(
    left: &mut L,
    right: &mut R,
) -> Result<(), String> {
    let mut left_buf = [0_u8; 16 * 1024];
    let mut right_buf = [0_u8; 16 * 1024];
    loop {
        let mut made_progress = false;
        match left.read(&mut left_buf) {
            Ok(0) => break,
            Ok(read) => {
                right
                    .write_all(&left_buf[..read])
                    .map_err(|error| format!("Write failed: {error}"))?;
                right
                    .flush()
                    .map_err(|error| format!("Flush failed: {error}"))?;
                made_progress = true;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(format!("Read failed: {error}")),
        }

        match right.read(&mut right_buf) {
            Ok(0) => break,
            Ok(read) => {
                left.write_all(&right_buf[..read])
                    .map_err(|error| format!("Write failed: {error}"))?;
                left.flush()
                    .map_err(|error| format!("Flush failed: {error}"))?;
                made_progress = true;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(format!("Read failed: {error}")),
        }

        if !made_progress {
            thread::sleep(Duration::from_millis(2));
        }
    }
    Ok(())
}

fn run_listener(inner: Arc<RemoteHostInner>) {
    let config = inner
        .config
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let bind = format!("{}:{}", config.bind_address, config.port);
    let listener = match TcpListener::bind(&bind) {
        Ok(listener) => listener,
        Err(error) => {
            inner.listener_running.store(false, Ordering::Relaxed);
            if let Ok(mut slot) = inner.listener_error.write() {
                *slot = Some(format!("Could not listen on {bind}: {error}"));
            }
            set_last_connection_note(
                &inner,
                format!("Remote host could not start listening on {bind}: {error}"),
                true,
            );
            eprintln!("[remote] failed to bind {bind}: {error}");
            return;
        }
    };
    inner.listener_running.store(true, Ordering::Relaxed);
    if let Ok(mut slot) = inner.listener_error.write() {
        *slot = None;
    }
    let _ = listener.set_nonblocking(true);

    while !inner.stop_flag.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let connection_id = inner.next_connection_id.fetch_add(1, Ordering::Relaxed);
                let thread_inner = inner.clone();
                thread::spawn(move || {
                    handle_client_connection(thread_inner, connection_id, stream)
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
    inner.listener_running.store(false, Ordering::Relaxed);
}

fn run_broadcaster(inner: Arc<RemoteHostInner>) {
    let mut last_snapshot_revision = 0_u64;
    let mut last_controller_client_id: Option<String> = None;

    while !inner.stop_flag.load(Ordering::Relaxed) {
        let connected_clients = inner
            .clients
            .lock()
            .map(|clients| clients.len())
            .unwrap_or(0);
        if connected_clients == 0 {
            thread::sleep(IDLE_BROADCAST_INTERVAL);
            continue;
        }

        let snapshot_revision = inner.snapshot_revision.load(Ordering::Relaxed);
        let controller_client_id = inner
            .controller_client_id
            .read()
            .map(|slot| slot.clone())
            .unwrap_or_default();
        if snapshot_revision == last_snapshot_revision
            && controller_client_id == last_controller_client_id
        {
            thread::sleep(SNAPSHOT_BROADCAST_INTERVAL);
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
        let app_hash = stable_hash(&app_state);
        let runtime_hash = stable_hash(&runtime_state);
        let port_hash = stable_hash(&port_statuses);

        let Ok(mut clients) = inner.clients.lock() else {
            thread::sleep(SNAPSHOT_BROADCAST_INTERVAL);
            continue;
        };
        let mut dead_connections = Vec::new();

        for (connection_id, client) in clients.iter_mut() {
            let you_have_control =
                controller_client_id.as_deref() == Some(client.client_id.as_str());
            let app_changed = client.last_app_hash != app_hash;
            let runtime_changed = client.last_runtime_hash != runtime_hash;
            let port_changed = client.last_port_hash != port_hash;
            let controller_changed = client.last_controller_client_id != controller_client_id
                || client.last_you_have_control != you_have_control;

            if !app_changed && !runtime_changed && !port_changed && !controller_changed {
                continue;
            }

            let delta = RemoteWorkspaceDelta {
                app_state: app_changed.then_some(app_state.clone()),
                runtime_state: runtime_changed.then_some(runtime_state.clone()),
                port_statuses: port_changed.then_some(port_statuses.clone()),
                controller_client_id: controller_client_id.clone(),
                you_have_control,
            };

            if client.sender.send(ServerMessage::Delta { delta }).is_err() {
                dead_connections.push(*connection_id);
                continue;
            }

            client.last_app_hash = app_hash;
            client.last_runtime_hash = runtime_hash;
            client.last_port_hash = port_hash;
            client.last_controller_client_id = controller_client_id.clone();
            client.last_you_have_control = you_have_control;
        }

        for connection_id in dead_connections {
            clients.remove(&connection_id);
        }

        last_snapshot_revision = snapshot_revision;
        last_controller_client_id = controller_client_id;

        thread::sleep(SNAPSHOT_BROADCAST_INTERVAL);
    }
}

fn handle_client_connection(inner: Arc<RemoteHostInner>, connection_id: u64, stream: TcpStream) {
    let peer_label = stream
        .peer_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "unknown client".to_string());
    let config = inner
        .config
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let mut stream = match transport::accept_tls(stream, &config) {
        Ok(stream) => stream,
        Err(error) => {
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

    let hello = match read_message::<ClientMessage, _>(&mut stream) {
        Ok(message) => message,
        Err(error) => {
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

    if matches!(hello, ClientMessage::PortForwardHello { .. }) {
        if let Err(message) =
            handle_port_forward_connection(&inner, &peer_label, &mut stream, hello, &config)
        {
            set_last_connection_note(
                &inner,
                format!("Rejected port forward from {peer_label}: {message}"),
                true,
            );
            let _ = write_message(&mut stream, &ServerMessage::HelloErr { message });
        }
        return;
    }

    let (tx, rx) = mpsc::channel::<ServerMessage>();

    let (client_id, client_token) = match authenticate_client(&inner, hello) {
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
            let _ = write_message(&mut stream, &ServerMessage::HelloErr { message });
            return;
        }
    };

    let controller_client_id = inner
        .controller_client_id
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let you_have_control = controller_client_id.as_deref() == Some(client_id.as_str());
    let snapshot = current_snapshot(&inner, &client_id);
    let initial_subscriptions = session_ids_for_open_tabs(&snapshot.app_state);
    let app_hash = stable_hash(&snapshot.app_state);
    let runtime_hash = stable_hash(&snapshot.runtime_state);
    let port_hash = stable_hash(&snapshot.port_statuses);
    if let Ok(mut clients) = inner.clients.lock() {
        clients.insert(
            connection_id,
            ConnectedRemoteClient {
                client_id: client_id.clone(),
                sender: tx.clone(),
                subscribed_session_ids: initial_subscriptions,
                bootstrapped_session_ids: snapshot.session_views.keys().cloned().collect(),
                focused_session_id: snapshot.runtime_state.active_session_id.clone(),
                last_app_hash: app_hash,
                last_runtime_hash: runtime_hash,
                last_port_hash: port_hash,
                last_controller_client_id: controller_client_id.clone(),
                last_you_have_control: you_have_control,
            },
        );
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
    if let Err(error) = write_message(&mut stream, &hello_ok) {
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
            clients.remove(&connection_id);
        }
        return;
    }
    set_last_connection_note(
        &inner,
        format!("Remote client {client_id} connected from {peer_label}."),
        false,
    );

    while !inner.stop_flag.load(Ordering::Relaxed) {
        let mut should_break = false;
        while let Ok(message) = rx.try_recv() {
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
        if should_break {
            break;
        }

        match try_read_message::<ClientMessage, _>(&mut stream, &mut read_buffer) {
            Ok(Some(ClientMessage::SetFocusedSession { session_id })) => {
                if let Ok(mut clients) = inner.clients.lock() {
                    if let Some(client) = clients.get_mut(&connection_id) {
                        client.focused_session_id = session_id.clone();
                    }
                }
                if let Some(session_id) = session_id {
                    if let Ok(handler) = inner.focused_session_handler.read() {
                        if let Some(handler) = handler.as_ref() {
                            handler(session_id);
                        }
                    }
                }
            }
            Ok(Some(ClientMessage::SubscribeSessions { session_ids })) => {
                let bootstraps = inner
                    .session_bootstrap_provider
                    .read()
                    .ok()
                    .and_then(|provider| provider.as_ref().cloned())
                    .map(|provider| {
                        session_ids
                            .iter()
                            .filter_map(|session_id| {
                                provider(session_id)
                                    .map(|bootstrap| (session_id.clone(), bootstrap))
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if let Ok(mut clients) = inner.clients.lock() {
                    if let Some(client) = clients.get_mut(&connection_id) {
                        for session_id in &session_ids {
                            client.subscribed_session_ids.insert(session_id.clone());
                        }
                    }
                }
                let mut bootstrapped_session_ids = Vec::new();
                for (session_id, bootstrap) in bootstraps {
                    if tx
                        .send(ServerMessage::SessionStream {
                            event: RemoteSessionStreamEvent::Bootstrap { bootstrap },
                        })
                        .is_ok()
                    {
                        bootstrapped_session_ids.push(session_id);
                    }
                }
                if let Ok(mut clients) = inner.clients.lock() {
                    if let Some(client) = clients.get_mut(&connection_id) {
                        for session_id in bootstrapped_session_ids {
                            client.bootstrapped_session_ids.insert(session_id);
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
                        }
                    }
                }
            }
            Ok(Some(ClientMessage::Action { action })) => {
                if requires_control(&action) && !current_controller_allows(&inner, &client_id) {
                    continue;
                }
                if let Ok(mut requests) = inner.pending_requests.lock() {
                    requests.push(PendingRemoteRequest {
                        client_id: client_id.clone(),
                        action,
                        response: None,
                    });
                }
            }
            Ok(Some(ClientMessage::TakeControl)) => {
                if let Ok(mut controller) = inner.controller_client_id.write() {
                    *controller = Some(client_id.clone());
                }
            }
            Ok(Some(ClientMessage::ReleaseControl)) => {
                if let Ok(mut controller) = inner.controller_client_id.write() {
                    if controller.as_deref() == Some(client_id.as_str()) {
                        *controller = None;
                    }
                }
            }
            Ok(Some(ClientMessage::Ping)) => {
                if write_message(&mut stream, &ServerMessage::Pong).is_err() {
                    break;
                }
            }
            Ok(Some(ClientMessage::TerminalInput {
                input,
                enqueued_at_epoch_ms,
            })) => {
                if current_controller_allows(&inner, &client_id) {
                    if let Ok(handler) = inner.terminal_input_handler.read() {
                        if let Some(handler) = handler.as_ref() {
                            let _ = handler(input, enqueued_at_epoch_ms);
                        }
                    }
                }
            }
            Ok(Some(ClientMessage::ResizeSession {
                session_id,
                dimensions,
            })) => {
                if current_controller_allows(&inner, &client_id) {
                    if let Ok(handler) = inner.terminal_resize_handler.read() {
                        if let Some(handler) = handler.as_ref() {
                            handler(session_id, dimensions);
                        }
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

                let (response_tx, response_rx) = mpsc::channel();
                if let Ok(mut requests) = inner.pending_requests.lock() {
                    requests.push(PendingRemoteRequest {
                        client_id: client_id.clone(),
                        action,
                        response: Some(response_tx),
                    });
                }
                let result = response_rx
                    .recv_timeout(REQUEST_TIMEOUT)
                    .unwrap_or_else(|_| RemoteActionResult::error("Remote host timed out."));
                let _ = tx.send(ServerMessage::Response { request_id, result });
            }
            Ok(Some(ClientMessage::Disconnect)) => break,
            Ok(Some(ClientMessage::Hello { .. } | ClientMessage::PortForwardHello { .. })) => break,
            Ok(None) => {
                thread::sleep(Duration::from_millis(12));
            }
            Err(_) => break,
        }
    }

    let _ = stream.sock.shutdown(Shutdown::Both);
    if let Ok(mut clients) = inner.clients.lock() {
        clients.remove(&connection_id);
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
    if let Ok(_update_guard) = inner.config_update_lock.lock() {
        if let Ok(mut config) = inner.config.write() {
            if let Some(client) = config
                .paired_clients
                .iter_mut()
                .find(|client| client.client_id == client_id)
            {
                client.last_seen_epoch_ms = Some(now_epoch_ms());
                bump_host_config_revision(&inner);
            }
        }
    }
}

fn authenticate_client(
    inner: &Arc<RemoteHostInner>,
    hello: ClientMessage,
) -> Result<(String, String), String> {
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

    let _update_guard = inner
        .config_update_lock
        .lock()
        .map_err(|_| "Host config update unavailable.".to_string())?;
    let mut config = inner
        .config
        .write()
        .map_err(|_| "Host config is unavailable.".to_string())?;
    match auth {
        ClientAuth::PairToken { token } => {
            if token.trim() != config.pairing_token.trim() {
                return Err("Pairing token did not match the host.".to_string());
            }
            let client_id = generate_secret("client");
            let client_token = generate_secret("auth");
            config.paired_clients.push(PairedRemoteClient {
                client_id: client_id.clone(),
                label: client_label,
                auth_token: client_token.clone(),
                last_seen_epoch_ms: Some(now_epoch_ms()),
            });
            bump_host_config_revision(inner);
            Ok((client_id, client_token))
        }
        ClientAuth::ClientToken {
            client_id,
            auth_token,
        } => {
            let Some(client) = config
                .paired_clients
                .iter_mut()
                .find(|client| client.client_id == client_id && client.auth_token == auth_token)
            else {
                return Err("Saved remote credentials are no longer valid.".to_string());
            };
            client.last_seen_epoch_ms = Some(now_epoch_ms());
            bump_host_config_revision(inner);
            Ok((client.client_id.clone(), client.auth_token.clone()))
        }
    }
}

fn handle_port_forward_connection(
    inner: &Arc<RemoteHostInner>,
    peer_label: &str,
    stream: &mut transport::ServerTlsStream,
    hello: ClientMessage,
    config: &RemoteHostConfig,
) -> Result<(), String> {
    let (client_id, requested_port) = authenticate_port_forward(inner, hello, config)?;
    let mut upstream = TcpStream::connect(("127.0.0.1", requested_port))
        .or_else(|_| TcpStream::connect(("::1", requested_port)))
        .map_err(|error| {
            format!("Could not connect to host localhost:{requested_port}: {error}")
        })?;
    let _ = upstream.set_nodelay(true);
    let _ = upstream.set_read_timeout(Some(Duration::from_millis(40)));
    let _ = upstream.set_write_timeout(Some(Duration::from_secs(5)));
    write_message(stream, &ServerMessage::PortForwardOk)
        .map_err(|error| format!("Could not start port forward: {error}"))?;
    if let Err(error) = copy_bidirectional(&mut upstream, stream) {
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
    config: &RemoteHostConfig,
) -> Result<(String, u16), String> {
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
    if !host_can_forward_port(inner, requested_port) {
        return Err(format!(
            "Port {requested_port} is not a live DevManager server port on this host."
        ));
    }
    Ok((client_id, requested_port))
}

fn host_can_forward_port(inner: &Arc<RemoteHostInner>, requested_port: u16) -> bool {
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

    for project in app_state.projects() {
        for folder in &project.folders {
            for command in &folder.commands {
                if command.port != Some(requested_port) {
                    continue;
                }
                let Some(session) = runtime_state.sessions.get(&command.id) else {
                    continue;
                };
                let Some(status) = port_statuses.get(&requested_port) else {
                    continue;
                };
                if session.status.is_live() && status.in_use && runtime_owns_port(session, status) {
                    return true;
                }
            }
        }
    }
    false
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

pub(crate) fn current_controller_allows(inner: &Arc<RemoteHostInner>, client_id: &str) -> bool {
    inner
        .controller_client_id
        .read()
        .ok()
        .and_then(|controller| controller.clone())
        .is_some_and(|controller| controller == client_id)
}

fn runtime_owns_port(session: &SessionRuntimeState, status: &PortStatus) -> bool {
    let Some(pid) = status.pid else {
        return false;
    };

    if session.pid == Some(pid) {
        return true;
    }

    session.resources.process_ids.contains(&pid)
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

fn run_client_connection(
    mut stream: transport::ClientTlsStream,
    rx: mpsc::Receiver<ClientMessage>,
    inner: Arc<RemoteClientInner>,
) {
    let mut read_buffer = Vec::new();
    let mut last_heartbeat_at = Instant::now();

    while inner
        .disconnected_message
        .read()
        .ok()
        .and_then(|message| message.clone())
        .is_none()
    {
        let mut should_break = false;
        while let Ok(message) = rx.try_recv() {
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
                        let handle = RemoteClientHandle {
                            inner: inner.clone(),
                        };
                        handle.note_output_received(emitted_at_epoch_ms);
                        if let Ok(replicas) = inner.session_replicas.read() {
                            if let Some(replica) = replicas.get(&session_id) {
                                replica.apply_output_bytes(&bytes);
                            }
                        }
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
            Ok(None) => thread::sleep(Duration::from_millis(12)),
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

fn read_message<T: for<'de> Deserialize<'de>, R: Read>(stream: &mut R) -> Result<T, String> {
    let mut buffer = Vec::new();
    loop {
        if let Some(message) = try_read_message(stream, &mut buffer)? {
            return Ok(message);
        }
        thread::sleep(Duration::from_millis(10));
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

pub(crate) fn current_snapshot(
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
    let subscribed_session_ids = session_ids_for_open_tabs(&app_state);
    let session_views = inner
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
    let port_statuses = inner
        .port_statuses
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
        session_views,
        port_statuses,
        you_have_control: controller_client_id.as_deref() == Some(client_id),
        controller_client_id,
        server_id: config.server_id,
    }
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
    snapshot.controller_client_id = delta.controller_client_id;
    snapshot.you_have_control = delta.you_have_control;
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::{now_epoch_ms, remote_state_path};
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    static TEST_PROFILE_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) struct TestProfileGuard {
        previous_profile: Option<String>,
        remote_state_dir: PathBuf,
        _lock: MutexGuard<'static, ()>,
    }

    impl TestProfileGuard {
        pub(crate) fn new(label: &str) -> Self {
            let lock = TEST_PROFILE_LOCK.lock().expect("profile lock");
            let previous_profile = std::env::var("DEVMANAGER_PROFILE").ok();
            let profile = format!("{label}-{}-{}", std::process::id(), now_epoch_ms());
            std::env::set_var("DEVMANAGER_PROFILE", &profile);
            let remote_state_dir = remote_state_path()
                .expect("remote state path")
                .parent()
                .expect("remote state dir")
                .to_path_buf();
            let _ = std::fs::remove_dir_all(&remote_state_dir);
            Self {
                previous_profile,
                remote_state_dir,
                _lock: lock,
            }
        }
    }

    impl Drop for TestProfileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.remote_state_dir);
            if let Some(previous_profile) = self.previous_profile.as_ref() {
                std::env::set_var("DEVMANAGER_PROFILE", previous_profile);
            } else {
                std::env::remove_var("DEVMANAGER_PROFILE");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::TestProfileGuard;
    use super::{
        apply_workspace_delta, current_controller_allows, current_snapshot,
        format_handshake_stage_error, generate_pairing_token, load_remote_machine_state,
        now_epoch_ms, save_remote_machine_state, set_last_connection_note, upsert_known_host,
        ClientAuth, ConnectedRemoteClient, KnownRemoteHost, LocalPortForwardManager,
        PairedRemoteClient, PairedWebClient, RemoteClientHandle, RemoteClientInner,
        RemoteHostConfig, RemoteHostService, RemoteLatencyStats, RemoteMachineState,
        RemoteSessionBootstrap, RemoteSessionStreamEvent, RemoteWorkspaceDelta,
        RemoteWorkspaceSnapshot, ServerMessage,
    };
    use crate::models::{PortStatus, SessionTab, TabType};
    use crate::state::{AppState, RuntimeState, SessionDimensions, SessionRuntimeState};
    use crate::terminal::session::{TerminalBackend, TerminalScreenSnapshot, TerminalSessionView};
    use std::collections::{HashMap, HashSet};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{mpsc, Arc, Mutex, RwLock};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn pairing_token_is_short_and_non_empty() {
        let token = generate_pairing_token();
        assert!(!token.is_empty());
        assert!(token.len() >= 4);
    }

    #[test]
    fn remote_machine_defaults_include_host_config() {
        let state = RemoteMachineState::default();
        assert!(!state.host.server_id.is_empty());
        assert!(!state.host.pairing_token.is_empty());
        assert_eq!(state.host.port, 43871);
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
            label: "Phone".to_string(),
            issued_at_epoch_ms: Some(10),
            last_seen_epoch_ms: Some(20),
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
    fn revoke_paired_client_removes_saved_token_and_control() {
        let mut config = RemoteHostConfig::default();
        config.paired_clients.push(PairedRemoteClient {
            client_id: "client-1".to_string(),
            label: "Laptop".to_string(),
            auth_token: "secret".to_string(),
            last_seen_epoch_ms: Some(1),
        });
        let service = RemoteHostService::new(config);
        if let Ok(mut controller) = service.inner.controller_client_id.write() {
            *controller = Some("client-1".to_string());
        }

        assert!(service.revoke_paired_client("client-1"));
        assert!(service.config().paired_clients.is_empty());
        assert!(service.status().controller_client_id.is_none());
    }

    #[test]
    fn revoke_paired_web_client_disconnects_live_browser_and_clears_control() {
        let mut config = RemoteHostConfig::default();
        config.web.paired_clients.push(PairedWebClient {
            client_id: "web-client-1".to_string(),
            label: "Browser".to_string(),
            issued_at_epoch_ms: Some(1),
            last_seen_epoch_ms: Some(1),
        });
        let service = RemoteHostService::new(config);
        let (tx, rx) = std::sync::mpsc::channel();

        if let Ok(mut clients) = service.inner.clients.lock() {
            clients.insert(
                1,
                ConnectedRemoteClient {
                    client_id: "web-client-1".to_string(),
                    sender: tx,
                    subscribed_session_ids: HashSet::new(),
                    bootstrapped_session_ids: HashSet::new(),
                    focused_session_id: None,
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                },
            );
        }
        if let Ok(mut controller) = service.inner.controller_client_id.write() {
            *controller = Some("web-client-1".to_string());
        }

        assert!(service.revoke_paired_web_client("web-client-1"));
        assert!(service.config().web.paired_clients.is_empty());
        assert!(service.status().controller_client_id.is_none());
        match rx.recv().expect("disconnect message") {
            ServerMessage::Disconnected { message } => {
                assert!(message.contains("revoked"));
            }
            other => panic!("expected disconnected message, got {other:?}"),
        }
    }

    #[test]
    fn host_status_splits_live_native_and_web_clients() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (native_tx, _native_rx) = mpsc::channel();
        let (web_tx, _web_rx) = mpsc::channel();

        if let Ok(mut clients) = service.inner.clients.lock() {
            clients.insert(
                1,
                ConnectedRemoteClient {
                    client_id: "client-1".to_string(),
                    sender: native_tx,
                    subscribed_session_ids: HashSet::new(),
                    bootstrapped_session_ids: HashSet::new(),
                    focused_session_id: None,
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                },
            );
            clients.insert(
                2,
                ConnectedRemoteClient {
                    client_id: "web-client-1".to_string(),
                    sender: web_tx,
                    subscribed_session_ids: HashSet::new(),
                    bootstrapped_session_ids: HashSet::new(),
                    focused_session_id: None,
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
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
                    sender: subscribed_tx,
                    subscribed_session_ids: HashSet::from(["alpha".to_string()]),
                    bootstrapped_session_ids: HashSet::new(),
                    focused_session_id: Some("alpha".to_string()),
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
                },
            );
            clients.insert(
                2,
                ConnectedRemoteClient {
                    client_id: "client-2".to_string(),
                    sender: idle_tx,
                    subscribed_session_ids: HashSet::from(["beta".to_string()]),
                    bootstrapped_session_ids: HashSet::new(),
                    focused_session_id: Some("beta".to_string()),
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
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
    fn push_session_runtime_notifies_subscribed_clients() {
        let service = RemoteHostService::new(RemoteHostConfig::default());
        let (tx, rx) = mpsc::channel();
        if let Ok(mut clients) = service.inner.clients.lock() {
            clients.insert(
                1,
                ConnectedRemoteClient {
                    client_id: "client-1".to_string(),
                    sender: tx,
                    subscribed_session_ids: HashSet::from(["alpha".to_string()]),
                    bootstrapped_session_ids: HashSet::new(),
                    focused_session_id: Some("alpha".to_string()),
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
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
                    sender: tx,
                    subscribed_session_ids: HashSet::from(["alpha".to_string()]),
                    bootstrapped_session_ids: HashSet::new(),
                    focused_session_id: Some("alpha".to_string()),
                    last_app_hash: 0,
                    last_runtime_hash: 0,
                    last_port_hash: 0,
                    last_controller_client_id: None,
                    last_you_have_control: false,
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

        service.push_session_output("alpha", b"after-ready".to_vec());

        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ServerMessage::SessionStream {
                event: RemoteSessionStreamEvent::Bootstrap { bootstrap },
            }) => assert_eq!(bootstrap.session_id, "alpha"),
            other => panic!("expected bootstrap event, got {other:?}"),
        }

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
        let port = reserve_free_tcp_port();
        let mut config = RemoteHostConfig {
            enabled: true,
            bind_address: "127.0.0.1".to_string(),
            port,
            ..RemoteHostConfig::default()
        };
        let pair_token = config.pairing_token.clone();
        let expected_server_id = config.server_id.clone();
        let service = RemoteHostService::new(config.clone());

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

        assert_eq!(result.server_id, expected_server_id);
        assert!(!result.client_id.trim().is_empty());
        assert!(!result.client_token.trim().is_empty());
        assert!(!result.certificate_fingerprint.trim().is_empty());
        assert!(!result.you_have_control);
        assert_eq!(result.snapshot.server_id, expected_server_id);

        wait_for(
            || service.status().connected_clients == 1,
            Duration::from_secs(3),
            "host never registered connected client",
        );

        result.client.disconnect();

        wait_for(
            || service.status().connected_clients == 0,
            Duration::from_secs(3),
            "host never observed client disconnect",
        );

        config.enabled = false;
        service.apply_config(config);
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
    fn port_forward_tunnels_bytes_to_a_live_managed_server_port() {
        let host_port = reserve_free_tcp_port();
        let server_port = reserve_free_tcp_port();
        let server_ready = Arc::new(AtomicBool::new(false));
        let server_ready_signal = server_ready.clone();
        thread::spawn(move || {
            let listener =
                TcpListener::bind(("127.0.0.1", server_port)).expect("echo server should bind");
            server_ready_signal.store(true, Ordering::SeqCst);
            let (mut socket, _) = listener.accept().expect("echo server should accept");
            let mut buf = [0_u8; 4];
            socket
                .read_exact(&mut buf)
                .expect("echo server should read ping");
            assert_eq!(&buf, b"ping");
            socket
                .write_all(b"pong")
                .expect("echo server should write pong");
        });
        wait_for(
            || server_ready.load(Ordering::Relaxed),
            Duration::from_secs(3),
            "echo server never started",
        );

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

        let mut forwarded = client
            .client
            .open_port_forward(server_port)
            .expect("port forward should open");
        forwarded
            .write_all(b"ping")
            .expect("forwarded stream should write");
        let mut buf = [0_u8; 4];
        forwarded
            .read_exact(&mut buf)
            .expect("forwarded stream should read");
        assert_eq!(&buf, b"pong");

        client.client.disconnect();
        config.enabled = false;
        service.apply_config(config);
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
        RemoteClientHandle {
            inner: Arc::new(RemoteClientInner {
                outgoing: tx,
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
            }),
        }
    }
}
