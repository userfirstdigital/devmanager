mod client_pool;
mod transport;

pub use client_pool::RemoteClientPool;

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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PROTOCOL_VERSION: u32 = 2;
const REMOTE_FILE_NAME: &str = "remote.json";
const SNAPSHOT_BROADCAST_INTERVAL: Duration = Duration::from_millis(33);
const IDLE_BROADCAST_INTERVAL: Duration = Duration::from_millis(250);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

type SessionBootstrapProvider = Arc<dyn Fn(&str) -> Option<RemoteSessionBootstrap> + Send + Sync>;
type TerminalInputHandler = Arc<dyn Fn(RemoteTerminalInput, u64) + Send + Sync>;
type TerminalResizeHandler = Arc<dyn Fn(String, SessionDimensions) + Send + Sync>;

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
    pub server_id: String,
    pub pairing_token: String,
    pub certificate_pem: String,
    pub private_key_pem: String,
    pub certificate_fingerprint: String,
    pub paired_clients: Vec<PairedRemoteClient>,
}

impl Default for RemoteHostConfig {
    fn default() -> Self {
        let mut config = Self {
            enabled: false,
            bind_address: "0.0.0.0".to_string(),
            port: 43871,
            server_id: generate_secret("host"),
            pairing_token: generate_pairing_token(),
            certificate_pem: String::new(),
            private_key_pem: String::new(),
            certificate_fingerprint: String::new(),
            paired_clients: Vec::new(),
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
    HelloErr {
        message: String,
    },
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
    Disconnected {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RemoteTerminalInput {
    Text { session_id: String, text: String },
    Bytes { session_id: String, bytes: Vec<u8> },
    Paste { session_id: String, text: String },
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
    pub bind_address: String,
    pub port: u16,
    pub pairing_token: String,
    pub connected_clients: usize,
    pub controller_client_id: Option<String>,
    pub listening: bool,
    pub listener_error: Option<String>,
    pub last_connection_note: Option<String>,
    pub last_connection_is_error: bool,
    pub latency: RemoteLatencyStats,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteLatencyStats {
    pub input_enqueue_to_host_write_ms: Option<u64>,
    pub output_host_to_client_ms: Option<u64>,
    pub output_client_to_paint_ms: Option<u64>,
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

struct RemoteHostInner {
    config: RwLock<RemoteHostConfig>,
    config_revision: AtomicU64,
    snapshot_revision: AtomicU64,
    shared_state: RwLock<AppState>,
    runtime_state: RwLock<RuntimeState>,
    port_statuses: RwLock<HashMap<u16, PortStatus>>,
    session_bootstrap_provider: RwLock<Option<SessionBootstrapProvider>>,
    terminal_input_handler: RwLock<Option<TerminalInputHandler>>,
    terminal_resize_handler: RwLock<Option<TerminalResizeHandler>>,
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
}

#[derive(Clone)]
struct ConnectedRemoteClient {
    client_id: String,
    sender: mpsc::Sender<ServerMessage>,
    subscribed_session_ids: HashSet<String>,
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
    client_id: String,
    client_token: String,
    server_id: String,
    certificate_fingerprint: String,
}

impl RemoteHostService {
    pub fn new(config: RemoteHostConfig) -> Self {
        let mut config = config;
        let _ = transport::ensure_host_tls_material(&mut config);
        let service = Self {
            inner: Arc::new(RemoteHostInner {
                config: RwLock::new(config.clone()),
                config_revision: AtomicU64::new(1),
                snapshot_revision: AtomicU64::new(1),
                shared_state: RwLock::new(AppState::default()),
                runtime_state: RwLock::new(RuntimeState::default()),
                port_statuses: RwLock::new(HashMap::new()),
                session_bootstrap_provider: RwLock::new(None),
                terminal_input_handler: RwLock::new(None),
                terminal_resize_handler: RwLock::new(None),
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
            }),
        };
        service.apply_config(config);
        service
    }

    pub fn apply_config(&self, config: RemoteHostConfig) {
        let mut config = config;
        let _ = transport::ensure_host_tls_material(&mut config);
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
        if let Ok(mut slot) = self.inner.shared_state.write() {
            *slot = app_state;
        }
        if let Ok(mut slot) = self.inner.runtime_state.write() {
            *slot = runtime_state;
        }
        if let Ok(mut slot) = self.inner.port_statuses.write() {
            *slot = port_statuses;
        }
        self.inner.snapshot_revision.fetch_add(1, Ordering::Relaxed);
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

    pub fn push_session_output(&self, session_id: &str, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
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
                    chunk_seq: self.inner.next_output_chunk_seq.fetch_add(1, Ordering::Relaxed),
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
        let Ok(mut clients) = self.inner.clients.lock() else {
            return;
        };
        let mut dead_connections = Vec::new();
        for (connection_id, client) in clients.iter_mut() {
            if !client.subscribed_session_ids.contains(session_id) {
                continue;
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
        let (enabled, bind_address, port, pairing_token) = self
            .inner
            .config
            .read()
            .map(|config| {
                (
                    config.enabled,
                    config.bind_address.clone(),
                    config.port,
                    config.pairing_token.clone(),
                )
            })
            .unwrap_or_default();
        let connected_clients = self
            .inner
            .clients
            .lock()
            .map(|clients| clients.len())
            .unwrap_or(0);
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
            bind_address,
            port,
            pairing_token,
            connected_clients,
            controller_client_id,
            listening,
            listener_error,
            last_connection_note,
            last_connection_is_error,
            latency,
        }
    }

    pub fn revoke_paired_client(&self, client_id: &str) -> bool {
        let mut removed = false;
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
        self.inner.stop_flag.store(false, Ordering::SeqCst);

        let config = self
            .inner
            .config
            .read()
            .map(|slot| slot.clone())
            .unwrap_or_default();
        if !config.enabled {
            return;
        }

        let listener_inner = self.inner.clone();
        let listener_thread = thread::spawn(move || run_listener(listener_inner));
        if let Ok(mut handle) = self.inner.listener_thread.lock() {
            *handle = Some(listener_thread);
        }

        let broadcaster_inner = self.inner.clone();
        let broadcaster_thread = thread::spawn(move || run_broadcaster(broadcaster_inner));
        if let Ok(mut handle) = self.inner.broadcaster_thread.lock() {
            *handle = Some(broadcaster_thread);
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
            client_id: client_id.clone(),
            client_token: client_token.clone(),
            server_id: server_id.clone(),
            certificate_fingerprint: certificate_fingerprint.clone(),
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
        let _ = self
            .inner
            .outgoing
            .send(ClientMessage::TerminalInput {
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
    let (tx, rx) = mpsc::channel::<ServerMessage>();

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
                        client.focused_session_id = session_id;
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
                            .filter_map(|session_id| provider(session_id))
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
                for bootstrap in bootstraps {
                    let _ = tx.send(ServerMessage::SessionStream {
                        event: RemoteSessionStreamEvent::Bootstrap {
                            bootstrap,
                        },
                    });
                }
            }
            Ok(Some(ClientMessage::UnsubscribeSessions { session_ids })) => {
                if let Ok(mut clients) = inner.clients.lock() {
                    if let Some(client) = clients.get_mut(&connection_id) {
                        for session_id in &session_ids {
                            client.subscribed_session_ids.remove(session_id);
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
            Ok(Some(ClientMessage::TerminalInput {
                input,
                enqueued_at_epoch_ms,
            })) => {
                if current_controller_allows(&inner, &client_id) {
                    if let Ok(handler) = inner.terminal_input_handler.read() {
                        if let Some(handler) = handler.as_ref() {
                            handler(input, enqueued_at_epoch_ms);
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
            Ok(Some(ClientMessage::Hello { .. })) => break,
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

fn current_controller_allows(inner: &Arc<RemoteHostInner>, client_id: &str) -> bool {
    inner
        .controller_client_id
        .read()
        .ok()
        .and_then(|controller| controller.clone())
        .is_some_and(|controller| controller == client_id)
}

fn requires_control(action: &RemoteAction) -> bool {
    !matches!(
        action,
        RemoteAction::SearchSession { .. }
            | RemoteAction::BrowsePath { .. }
            | RemoteAction::ListDirectory { .. }
            | RemoteAction::StatPath { .. }
            | RemoteAction::ReadTextFile { .. }
            | RemoteAction::ScanFolder { .. }
            | RemoteAction::ScanRoot { .. }
            | RemoteAction::ExportSessionText { .. }
    )
}

fn run_client_connection(
    mut stream: transport::ClientTlsStream,
    rx: mpsc::Receiver<ClientMessage>,
    inner: Arc<RemoteClientInner>,
) {
    let mut read_buffer = Vec::new();

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
                                snapshot
                                    .session_views
                                    .insert(
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
                    RemoteSessionStreamEvent::RuntimePatch { session_id, runtime }
                    | RemoteSessionStreamEvent::Closed { session_id, runtime } => {
                        if let Ok(replicas) = inner.session_replicas.read() {
                            if let Some(replica) = replicas.get(&session_id) {
                                replica.apply_runtime(runtime.clone());
                            }
                        }
                        if let Ok(mut latest) = inner.latest_snapshot.write() {
                            if let Some(snapshot) = latest.as_mut() {
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
            Ok(Some(ServerMessage::HelloOk { .. } | ServerMessage::HelloErr { .. })) => {}
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

fn stable_hash<T: Serialize>(value: &T) -> u64 {
    let bytes = to_vec_named(value).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn current_snapshot(inner: &Arc<RemoteHostInner>, client_id: &str) -> RemoteWorkspaceSnapshot {
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
mod tests {
    use super::{
        apply_workspace_delta, current_controller_allows, current_snapshot,
        format_handshake_stage_error, generate_pairing_token, now_epoch_ms,
        set_last_connection_note,
        upsert_known_host, ClientAuth, ConnectedRemoteClient, PairedRemoteClient,
        RemoteClientHandle, RemoteClientInner, RemoteHostConfig, RemoteHostService,
        RemoteLatencyStats,
        RemoteMachineState, RemoteSessionBootstrap, RemoteSessionStreamEvent,
        RemoteWorkspaceDelta, RemoteWorkspaceSnapshot, ServerMessage,
    };
    use crate::models::{SessionTab, TabType};
    use crate::state::{AppState, RuntimeState, SessionDimensions, SessionRuntimeState};
    use crate::terminal::session::{TerminalBackend, TerminalScreenSnapshot, TerminalSessionView};
    use std::collections::{HashMap, HashSet};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
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
                event: RemoteSessionStreamEvent::Output {
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
                    RemoteSessionStreamEvent::Closed { session_id, runtime }
                    | RemoteSessionStreamEvent::RuntimePatch { session_id, runtime },
            }) => {
                assert_eq!(session_id, "alpha");
                assert_eq!(runtime.session_id, "alpha");
            }
            other => panic!("expected runtime stream event, got {other:?}"),
        }
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
                client_id: client_id.to_string(),
                client_token: "token-1".to_string(),
                server_id: "host-1".to_string(),
                certificate_fingerprint: "fingerprint-1".to_string(),
            }),
        }
    }
}
