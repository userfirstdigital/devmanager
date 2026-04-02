mod client_pool;
mod transport;

pub use client_pool::RemoteClientPool;

use crate::models::{
    PortStatus, Project, ProjectFolder, RootScanEntry, RunCommand, SSHConnection, ScanResult,
    Settings, TabType,
};
use crate::persistence::{self, PersistenceError};
use crate::state::{AppState, RuntimeState, SessionDimensions};
use crate::terminal::session::{TerminalSearchMatch, TerminalSessionView};
use rmp_serde::{decode::from_slice as from_messagepack_slice, encode::to_vec_named};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PROTOCOL_VERSION: u32 = 1;
const REMOTE_FILE_NAME: &str = "remote.json";
const SNAPSHOT_BROADCAST_INTERVAL: Duration = Duration::from_millis(180);
const IDLE_BROADCAST_INTERVAL: Duration = Duration::from_millis(750);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

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
    pub session_updates: HashMap<String, TerminalSessionView>,
    pub removed_session_ids: Vec<String>,
    pub port_statuses: Option<HashMap<u16, PortStatus>>,
    pub controller_client_id: Option<String>,
    pub you_have_control: bool,
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
    TerminalText {
        session_id: String,
        text: String,
    },
    TerminalBytes {
        session_id: String,
        bytes: Vec<u8>,
    },
    TerminalPaste {
        session_id: String,
        text: String,
    },
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
    ResizeSession {
        session_id: String,
        dimensions: SessionDimensions,
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
    SearchMatches { matches: Vec<TerminalSearchMatch> },
    BrowsePath { path: Option<String> },
    DirectoryEntries { entries: Vec<RemoteFsEntry> },
    PathStat { entry: Option<RemoteFsEntry> },
    TextFile { path: String, contents: String },
    RootScan { entries: Vec<RootScanEntry> },
    FolderScan { scan: ScanResult },
    ExportText { text: String },
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
    session_views: RwLock<HashMap<String, TerminalSessionView>>,
    port_statuses: RwLock<HashMap<u16, PortStatus>>,
    pending_requests: Mutex<Vec<PendingRemoteRequest>>,
    clients: Mutex<HashMap<u64, ConnectedRemoteClient>>,
    controller_client_id: RwLock<Option<String>>,
    listener_running: AtomicBool,
    listener_error: RwLock<Option<String>>,
    next_connection_id: AtomicU64,
    stop_flag: AtomicBool,
    listener_thread: Mutex<Option<thread::JoinHandle<()>>>,
    broadcaster_thread: Mutex<Option<thread::JoinHandle<()>>>,
}

#[derive(Clone)]
struct ConnectedRemoteClient {
    client_id: String,
    sender: mpsc::Sender<ServerMessage>,
    last_app_hash: u64,
    last_runtime_hash: u64,
    last_port_hash: u64,
    last_session_hashes: HashMap<String, u64>,
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
    disconnected_message: RwLock<Option<String>>,
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
                session_views: RwLock::new(HashMap::new()),
                port_statuses: RwLock::new(HashMap::new()),
                pending_requests: Mutex::new(Vec::new()),
                clients: Mutex::new(HashMap::new()),
                controller_client_id: RwLock::new(None),
                listener_running: AtomicBool::new(false),
                listener_error: RwLock::new(None),
                next_connection_id: AtomicU64::new(1),
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
        session_views: HashMap<String, TerminalSessionView>,
        port_statuses: HashMap<u16, PortStatus>,
    ) {
        if let Ok(mut slot) = self.inner.shared_state.write() {
            *slot = app_state;
        }
        if let Ok(mut slot) = self.inner.runtime_state.write() {
            *slot = runtime_state;
        }
        if let Ok(mut slot) = self.inner.session_views.write() {
            *slot = session_views;
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

    pub fn requested_session_ids(&self) -> Vec<String> {
        self.inner
            .session_views
            .read()
            .map(|views| views.keys().cloned().collect())
            .unwrap_or_default()
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
        RemoteHostStatus {
            enabled,
            bind_address,
            port,
            pairing_token,
            connected_clients,
            controller_client_id,
            listening,
            listener_error,
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
        write_message(&mut stream, &hello).map_err(|error| format!("Handshake failed: {error}"))?;
        let response: ServerMessage =
            read_message(&mut stream).map_err(|error| format!("Handshake failed: {error}"))?;
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
        let inner = Arc::new(RemoteClientInner {
            outgoing: tx.clone(),
            pending: Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(1),
            latest_snapshot: RwLock::new(Some(snapshot.clone())),
            disconnected_message: RwLock::new(None),
            client_id: client_id.clone(),
            client_token: client_token.clone(),
            server_id: server_id.clone(),
            certificate_fingerprint: certificate_fingerprint.clone(),
        });

        let reader_inner = inner.clone();
        thread::spawn(move || run_client_connection(stream, rx, reader_inner));

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

    pub fn send_terminal_input(&self, input: RemoteTerminalInput) {
        let _ = self
            .inner
            .outgoing
            .send(ClientMessage::TerminalInput { input });
    }

    pub fn send_action(&self, action: RemoteAction) {
        let _ = self.inner.outgoing.send(ClientMessage::Action { action });
    }

    pub fn take_control(&self) {
        let _ = self.inner.outgoing.send(ClientMessage::TakeControl);
    }

    pub fn release_control(&self) {
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
                thread::sleep(Duration::from_millis(60));
            }
            Err(_) => thread::sleep(Duration::from_millis(120)),
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
        let session_views = inner
            .session_views
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
        let session_hashes = session_views
            .iter()
            .map(|(session_id, view)| (session_id.clone(), stable_hash(view)))
            .collect::<HashMap<_, _>>();

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

            let mut session_updates = HashMap::new();
            for (session_id, view) in session_views.iter() {
                if client.last_session_hashes.get(session_id) != session_hashes.get(session_id) {
                    session_updates.insert(session_id.clone(), view.clone());
                }
            }
            let removed_session_ids = client
                .last_session_hashes
                .keys()
                .filter(|session_id| !session_hashes.contains_key(*session_id))
                .cloned()
                .collect::<Vec<_>>();

            if !app_changed
                && !runtime_changed
                && !port_changed
                && !controller_changed
                && session_updates.is_empty()
                && removed_session_ids.is_empty()
            {
                continue;
            }

            let delta = RemoteWorkspaceDelta {
                app_state: app_changed.then_some(app_state.clone()),
                runtime_state: runtime_changed.then_some(runtime_state.clone()),
                session_updates,
                removed_session_ids,
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
            client.last_session_hashes = session_hashes.clone();
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
    let config = inner
        .config
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let mut stream = match transport::accept_tls(stream, &config) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("[remote] tls accept failed for connection {connection_id}: {error}");
            return;
        }
    };
    let mut read_buffer = Vec::new();
    let (tx, rx) = mpsc::channel::<ServerMessage>();

    let hello = match read_message::<ClientMessage, _>(&mut stream) {
        Ok(message) => message,
        Err(_) => return,
    };

    let (client_id, client_token) = match authenticate_client(&inner, hello) {
        Ok(auth) => auth,
        Err(message) => {
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
    let app_hash = stable_hash(&snapshot.app_state);
    let runtime_hash = stable_hash(&snapshot.runtime_state);
    let port_hash = stable_hash(&snapshot.port_statuses);
    let session_hashes = snapshot
        .session_views
        .iter()
        .map(|(session_id, view)| (session_id.clone(), stable_hash(view)))
        .collect::<HashMap<_, _>>();

    if let Ok(mut clients) = inner.clients.lock() {
        clients.insert(
            connection_id,
            ConnectedRemoteClient {
                client_id: client_id.clone(),
                sender: tx.clone(),
                last_app_hash: app_hash,
                last_runtime_hash: runtime_hash,
                last_port_hash: port_hash,
                last_session_hashes: session_hashes,
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
    if write_message(&mut stream, &hello_ok).is_err() {
        if let Ok(mut clients) = inner.clients.lock() {
            clients.remove(&connection_id);
        }
        return;
    }

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
            Ok(Some(ClientMessage::SetFocusedSession { .. })) => {}
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
            Ok(Some(ClientMessage::TerminalInput { input })) => {
                if current_controller_allows(&inner, &client_id) {
                    if let Ok(mut requests) = inner.pending_requests.lock() {
                        requests.push(PendingRemoteRequest {
                            client_id: client_id.clone(),
                            action: terminal_input_to_action(input),
                            response: None,
                        });
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

fn terminal_input_to_action(input: RemoteTerminalInput) -> RemoteAction {
    match input {
        RemoteTerminalInput::Text { session_id, text } => {
            RemoteAction::TerminalText { session_id, text }
        }
        RemoteTerminalInput::Bytes { session_id, bytes } => {
            RemoteAction::TerminalBytes { session_id, bytes }
        }
        RemoteTerminalInput::Paste { session_id, text } => {
            RemoteAction::TerminalPaste { session_id, text }
        }
    }
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
                if let Ok(mut latest) = inner.latest_snapshot.write() {
                    *latest = Some(snapshot);
                }
            }
            Ok(Some(ServerMessage::Delta { delta })) => {
                if let Ok(mut latest) = inner.latest_snapshot.write() {
                    let snapshot = latest.get_or_insert_with(RemoteWorkspaceSnapshot::default);
                    apply_workspace_delta(snapshot, delta);
                }
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
    let session_views = inner
        .session_views
        .read()
        .map(|slot| slot.clone())
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
    for session_id in delta.removed_session_ids {
        snapshot.session_views.remove(&session_id);
    }
    for (session_id, session_view) in delta.session_updates {
        snapshot.session_views.insert(session_id, session_view);
    }
    snapshot.controller_client_id = delta.controller_client_id;
    snapshot.you_have_control = delta.you_have_control;
}

#[cfg(test)]
mod tests {
    use super::{
        apply_workspace_delta, current_controller_allows, generate_pairing_token,
        upsert_known_host, PairedRemoteClient, RemoteHostConfig, RemoteHostService,
        RemoteMachineState, RemoteWorkspaceDelta, RemoteWorkspaceSnapshot,
    };
    use crate::state::{AppState, RuntimeState, SessionDimensions, SessionRuntimeState};
    use crate::terminal::session::{TerminalBackend, TerminalScreenSnapshot, TerminalSessionView};
    use std::collections::HashMap;
    use std::path::PathBuf;

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
                session_updates: HashMap::from([
                    ("keep".to_string(), session_view("keep-updated")),
                    ("new".to_string(), session_view("new")),
                ]),
                removed_session_ids: vec!["old".to_string()],
                controller_client_id: Some("client-1".to_string()),
                you_have_control: true,
                ..Default::default()
            },
        );

        assert!(!snapshot.session_views.contains_key("old"));
        assert_eq!(
            snapshot
                .session_views
                .get("keep")
                .map(|view| view.runtime.session_id.as_str()),
            Some("keep-updated")
        );
        assert!(snapshot.session_views.contains_key("new"));
        assert_eq!(snapshot.controller_client_id.as_deref(), Some("client-1"));
        assert!(snapshot.you_have_control);
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
}
