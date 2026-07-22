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

use crate::git::git_service::{
    AiCommitMessage, DeviceCodeResponse, GitBranch, GitDiffResult, GitLogEntry, GitStatusResult,
};
use crate::models::{
    PortStatus, Project, ProjectFolder, RootScanEntry, RunCommand, SSHConnection, ScanResult,
    Settings, TabType,
};
use crate::persistence::{self, PersistenceError};
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
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const PROTOCOL_VERSION: u32 = 5;
const REMOTE_FILE_NAME: &str = "remote.json";
const SNAPSHOT_BROADCAST_INTERVAL: Duration = Duration::from_millis(33);
const IDLE_BROADCAST_INTERVAL: Duration = Duration::from_millis(250);
const PENDING_BOOTSTRAP_RETRY_INTERVAL: Duration = Duration::from_millis(250);
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
    file.write_all(contents)
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
    if let Err(source) = fs::rename(&temp_path, &path) {
        let _ = fs::remove_file(&temp_path);
        return Err(PersistenceError::Io { path, source });
    }
    verify_remote_state_file_permissions(&path).map_err(|source| PersistenceError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(())
}

fn persist_host_config_snapshot(config: &RemoteHostConfig) -> Result<(), PersistenceError> {
    let _guard = REMOTE_STATE_SAVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut state = load_remote_machine_state_locked()?;
    state.host = config.clone();
    save_remote_machine_state_locked(&state)
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

struct RemoteHostServiceOwner {
    inner: Arc<RemoteHostInner>,
}

impl Drop for RemoteHostServiceOwner {
    fn drop(&mut self) {
        self.inner
            .native_runtime_generation
            .fetch_add(1, Ordering::SeqCst);
        self.inner.stop_flag.store(true, Ordering::SeqCst);

        let session_bootstrap_provider = self
            .inner
            .session_bootstrap_provider
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let terminal_input_handler = self
            .inner
            .terminal_input_handler
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let terminal_resize_handler = self
            .inner
            .terminal_resize_handler
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let focused_session_handler = self
            .inner
            .focused_session_handler
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let web_listener = self
            .inner
            .web_listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let listener_thread = self
            .inner
            .listener_thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let broadcaster_thread = self
            .inner
            .broadcaster_thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();

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
        if let Some(listener) = web_listener {
            listener.shutdown();
        }
        drain_web_clients_for_restart(&self.inner);

        if let Some(thread) = listener_thread {
            let _ = thread.join();
        }
        if let Some(thread) = broadcaster_thread {
            let _ = thread.join();
        }
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

pub(crate) struct RemoteHostInner {
    config: RwLock<RemoteHostConfig>,
    config_update_lock: Mutex<()>,
    config_revision: AtomicU64,
    /// Coordinates publication of workspace state with browser snapshot
    /// capture so a revision always describes the state sent with it.
    snapshot_state_lock: Mutex<()>,
    snapshot_revision: AtomicU64,
    runtime_instance_id: String,
    shared_state: RwLock<AppState>,
    runtime_state: RwLock<RuntimeState>,
    port_statuses: RwLock<HashMap<u16, PortStatus>>,
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
    listener_thread: Mutex<Option<thread::JoinHandle<()>>>,
    broadcaster_thread: Mutex<Option<thread::JoinHandle<()>>>,
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
        config.web.ensure_secrets();
        let _ = transport::ensure_host_tls_material(&mut config);
        let inner = Arc::new(RemoteHostInner {
            config: RwLock::new(config.clone()),
            config_update_lock: Mutex::new(()),
            config_revision: AtomicU64::new(1),
            snapshot_state_lock: Mutex::new(()),
            snapshot_revision: AtomicU64::new(1),
            runtime_instance_id: generate_secret("runtime"),
            shared_state: RwLock::new(AppState::default()),
            runtime_state: RwLock::new(RuntimeState::default()),
            port_statuses: RwLock::new(HashMap::new()),
            semantic_journals: Mutex::new(SemanticJournalStore::default()),
            semantic_publication_lock: Mutex::new(()),
            semantic_publication_generation: AtomicU64::new(0),
            #[cfg(test)]
            semantic_publication_test_hook: RwLock::new(None),
            semantic_delivery_lock: Mutex::new(()),
            #[cfg(test)]
            semantic_delivery_test_hook: RwLock::new(None),
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
            listener_thread: Mutex::new(None),
            broadcaster_thread: Mutex::new(None),
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
        let Ok(_update_guard) = self.inner.config_update_lock.lock() else {
            return;
        };
        if let Ok(mut slot) = self.inner.config.write() {
            *slot = config;
        }
        self.bump_config_revision();
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
        self.update_snapshot_parts(Some(app_state), Some(runtime_state), Some(port_statuses));
    }

    pub fn update_snapshot_parts(
        &self,
        app_state: Option<AppState>,
        runtime_state: Option<RuntimeState>,
        port_statuses: Option<HashMap<u16, PortStatus>>,
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
        self.inner
            .native_runtime_generation
            .fetch_add(1, Ordering::SeqCst);
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
        // Stop accepting browser connections first. Tokio shutdown may cancel
        // WebSocket tasks before their async unregister tail runs, so drain
        // any records left behind immediately afterwards. This ordering also
        // closes the narrow race where a new browser could register between a
        // pre-shutdown drain and runtime teardown.
        if let Ok(mut slot) = self.inner.web_listener.lock() {
            if let Some(handle) = slot.take() {
                handle.shutdown();
            }
        }
        drain_web_clients_for_restart(&self.inner);
        // Web-listener errors are scoped independently from native TCP state.
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
            let native_runtime_generation =
                self.inner.native_runtime_generation.load(Ordering::SeqCst);
            let listener_thread =
                thread::spawn(move || run_listener(listener_inner, native_runtime_generation));
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
        let timeout = request_timeout_for_action(&action);
        let request_id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        if let Ok(mut pending) = self.inner.pending.lock() {
            pending.insert(request_id, tx);
        }
        self.inner
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
            ServerMessage::PortForwardOk => {
                let _ = stream.sock.set_read_timeout(Some(Duration::from_secs(5)));
                Ok(stream)
            }
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
                let connection_stop_flag = stop_flag.clone();
                thread::spawn(move || {
                    handle_local_port_forward_connection(
                        connection_inner,
                        client,
                        port,
                        socket,
                        connection_stop_flag,
                    )
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
    stop_flag: Arc<AtomicBool>,
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
    let _ = remote_stream
        .sock
        .set_read_timeout(Some(Duration::from_millis(40)));

    if let Err(error) = copy_bidirectional(&mut local_socket, &mut remote_stream, || {
        stop_flag.load(Ordering::Acquire)
    }) {
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
    mut should_stop: impl FnMut() -> bool,
) -> Result<(), String> {
    let mut left_buf = [0_u8; 16 * 1024];
    let mut right_buf = [0_u8; 16 * 1024];
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

        if should_stop() {
            break;
        }
        match right.read(&mut right_buf) {
            Ok(0) => break,
            Ok(read) => {
                if should_stop() {
                    break;
                }
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

fn native_connection_should_stop(inner: &RemoteHostInner, native_runtime_generation: u64) -> bool {
    inner.stop_flag.load(Ordering::Acquire)
        || inner.native_runtime_generation.load(Ordering::Acquire) != native_runtime_generation
}

fn run_listener(inner: Arc<RemoteHostInner>, native_runtime_generation: u64) {
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

    while !native_connection_should_stop(&inner, native_runtime_generation) {
        match listener.accept() {
            Ok((stream, _)) => {
                let connection_id = inner.next_connection_id.fetch_add(1, Ordering::Relaxed);
                let thread_inner = inner.clone();
                thread::spawn(move || {
                    handle_client_connection(
                        thread_inner,
                        connection_id,
                        stream,
                        native_runtime_generation,
                    )
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
    let mut last_semantic_delivery_revision = 0_u64;
    let mut last_controller_client_id: Option<String> = None;
    let mut last_bootstrap_retry_at: HashMap<String, Instant> = HashMap::new();

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

        deliver_pending_bootstraps(&inner, &mut last_bootstrap_retry_at);

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
        let mut deliveries = Vec::new();

        for (connection_id, client) in clients.iter_mut() {
            let you_have_control =
                controller_client_id.as_deref() == Some(client.client_id.as_str());
            let app_changed = client.last_app_hash != app_hash;
            let runtime_changed = client.last_runtime_hash != runtime_hash;
            let port_changed = client.last_port_hash != port_hash;
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
                controller_client_id: controller_client_id.clone(),
                you_have_control,
            };

            client.last_app_hash = app_hash;
            client.last_runtime_hash = runtime_hash;
            client.last_port_hash = port_hash;
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

        thread::sleep(SNAPSHOT_BROADCAST_INTERVAL);
    }
}

pub(crate) fn deliver_pending_bootstraps(
    inner: &Arc<RemoteHostInner>,
    last_bootstrap_retry_at: &mut HashMap<String, Instant>,
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
            provider(session_id).map(|bootstrap| (session_id.clone(), bootstrap))
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

fn handle_client_connection(
    inner: Arc<RemoteHostInner>,
    connection_id: u64,
    stream: TcpStream,
    native_runtime_generation: u64,
) {
    let peer_addr = stream.peer_addr().ok();
    let peer_label = peer_addr
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| "unknown client".to_string());
    let peer_ip = peer_addr.map(|addr| addr.ip().to_string());
    let config = inner
        .config
        .read()
        .map(|slot| slot.clone())
        .unwrap_or_default();
    let mut stream = match transport::accept_tls(stream, &config, || {
        native_connection_should_stop(&inner, native_runtime_generation)
    }) {
        Ok(stream) => stream,
        Err(error) => {
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

    let hello = match read_message_until_cancelled::<ClientMessage, _, _>(&mut stream, || {
        native_connection_should_stop(&inner, native_runtime_generation)
    }) {
        Ok(message) => message,
        Err(error) => {
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

    if matches!(hello, ClientMessage::PortForwardHello { .. }) {
        if let Err(message) = handle_port_forward_connection(
            &inner,
            &peer_label,
            &mut stream,
            hello,
            native_runtime_generation,
        ) {
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

    let (client_id, client_token, client_label) = match authenticate_client(&inner, hello) {
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
    let snapshot = light_snapshot(&inner, &client_id);
    let app_hash = stable_hash(&snapshot.app_state);
    let runtime_hash = stable_hash(&snapshot.runtime_state);
    let port_hash = stable_hash(&snapshot.port_statuses);
    if let Ok(mut clients) = inner.clients.lock() {
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
                last_port_hash: port_hash,
                last_controller_client_id: controller_client_id.clone(),
                last_you_have_control: you_have_control,
                last_snapshot_revision: inner.snapshot_revision.load(Ordering::Relaxed),
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
    if let Err(error) = mutate_host_config(&inner, |config| {
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
                client_id: client_id.clone(),
                source: RemoteAccessSource::NativeApp,
                event_kind: if had_previous_connect {
                    RemoteAccessActivityKind::Reconnected
                } else {
                    RemoteAccessActivityKind::Connected
                },
                label: client_label.clone(),
                ip_address: peer_ip.clone(),
                event_at_epoch_ms: Some(now_epoch_ms()),
                browser_family: None,
                browser_version: None,
                os_family: None,
                device_class: Some("desktop".to_string()),
            },
        );
    }) {
        eprintln!(
            "[remote] failed to persist native access log for {client_id} from {peer_label}: {error}"
        );
    }
    set_last_connection_note(
        &inner,
        format!("Remote client {client_id} connected from {peer_label}."),
        false,
    );

    while !native_connection_should_stop(&inner, native_runtime_generation) {
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
                let client = config
                    .paired_clients
                    .iter_mut()
                    .find(|client| client.client_id == client_id && client.auth_token == auth_token)
                    .expect("serialized native client condition must remain true");
                client.label = client_label;
                client.last_seen_epoch_ms = Some(now_epoch_ms());
                (
                    client.client_id.clone(),
                    client.auth_token.clone(),
                    client.label.clone(),
                )
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
) -> Result<(), String> {
    let (client_id, auth_token, requested_port) = authenticate_port_forward(inner, hello)?;
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
                        let handle = RemoteClientHandle {
                            inner: inner.clone(),
                        };
                        handle.note_output_received(emitted_at_epoch_ms);
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
        generate_pairing_token, light_snapshot, load_remote_machine_state,
        native_connection_should_stop, now_epoch_ms, publish_semantic_event, read_message,
        request_timeout_for_action, requires_control, run_broadcaster, save_remote_known_hosts,
        save_remote_machine_state, set_last_connection_note, try_enqueue_pending_request,
        upsert_known_host, write_message, ClientAuth, ClientMessage, ConnectedRemoteClient,
        KnownRemoteHost, LocalPortForwardManager, PairedRemoteClient, PairedWebClient,
        PendingRemoteRequest, RemoteAccessActivityEvent, RemoteAccessActivityKind,
        RemoteAccessSource, RemoteAction, RemoteClientHandle, RemoteClientInner, RemoteHostConfig,
        RemoteHostService, RemoteHostWorkLimiter, RemoteLatencyStats, RemoteMachineState,
        RemoteSessionBootstrap, RemoteSessionStreamEvent, RemoteTerminalInput,
        RemoteWorkspaceDelta, RemoteWorkspaceSnapshot, ServerMessage, MAX_PENDING_REMOTE_REQUESTS,
    };
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
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex, RwLock};
    use std::thread;
    use std::time::{Duration, Instant};

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
    fn native_client_receives_output_while_bootstrap_lookup_blocks() {
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
    fn native_client_connections_are_recorded_in_activity_log() {
        let _profile = TestProfileGuard::new("native-activity-log");
        let port = reserve_free_tcp_port();
        let mut config = RemoteHostConfig {
            enabled: true,
            bind_address: "127.0.0.1".to_string(),
            port,
            ..RemoteHostConfig::default()
        };
        let pair_token = config.pairing_token.clone();
        let service = RemoteHostService::new(config.clone());

        wait_for(
            || service.status().listening,
            Duration::from_secs(3),
            "remote host never started listening",
        );

        let result = RemoteClientHandle::connect(
            "127.0.0.1",
            port,
            "Studio MacBook",
            ClientAuth::PairToken { token: pair_token },
            None,
        )
        .expect("loopback remote connect should succeed");

        wait_for(
            || {
                service.config().web.activity_log.iter().any(|event| {
                    event.source == RemoteAccessSource::NativeApp
                        && event.event_kind == RemoteAccessActivityKind::Connected
                        && event.label == "Studio MacBook"
                })
            },
            Duration::from_secs(3),
            "native client connection never appeared in activity log",
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
        let (payload_tx, payload_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        let upstream_thread = thread::spawn(move || {
            let (mut socket, _) = upstream.accept().expect("upstream should accept tunnel");
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("upstream read timeout should apply");
            let mut payload = [0_u8; 4];
            socket
                .read_exact(&mut payload)
                .expect("upstream should receive tunneled payload");
            payload_tx
                .send(payload)
                .expect("payload signal should send");
            let mut byte = [0_u8; 1];
            let closed = matches!(socket.read(&mut byte), Ok(0));
            closed_tx.send(closed).expect("closed signal should send");
        });

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
        assert!(matches!(
            read_message::<ServerMessage, _>(&mut stream).expect("host should answer hello"),
            ServerMessage::PortForwardOk
        ));
        stream
            .write_all(b"ping")
            .expect("active tunnel should accept payload");
        assert_eq!(
            payload_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("upstream did not receive tunneled payload"),
            *b"ping"
        );

        assert!(service.revoke_paired_client("active-client"));
        assert!(
            closed_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("active tunnel did not close after revocation"),
            "upstream did not observe EOF after client revocation"
        );
        upstream_thread.join().expect("upstream thread should exit");
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
    fn dropping_root_service_stops_an_active_native_port_forward() {
        let host_port = reserve_free_tcp_port();
        let server_port = reserve_free_tcp_port();
        let server_ready = Arc::new(AtomicBool::new(false));
        let server_ready_signal = server_ready.clone();
        let server_thread = thread::spawn(move || {
            let listener =
                TcpListener::bind(("127.0.0.1", server_port)).expect("server should bind");
            server_ready_signal.store(true, Ordering::SeqCst);
            let (mut socket, _) = listener.accept().expect("server should accept forward");
            socket
                .set_read_timeout(Some(Duration::from_millis(40)))
                .expect("server read timeout");
            let mut buffer = [0_u8; 64];
            loop {
                match socket.read(&mut buffer) {
                    Ok(0) => return,
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => return,
                }
            }
        });
        wait_for(
            || server_ready.load(Ordering::Relaxed),
            Duration::from_secs(3),
            "managed server never started",
        );

        let config = RemoteHostConfig {
            enabled: true,
            bind_address: "127.0.0.1".to_string(),
            port: host_port,
            ..RemoteHostConfig::default()
        };
        let pair_token = config.pairing_token.clone();
        let root = RemoteHostService::new(config);
        root.update_snapshot(
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
            || root.status().listening,
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
        let forwarded = client
            .client
            .open_port_forward(server_port)
            .expect("port forward should open");
        let inner = Arc::downgrade(&root.inner);

        drop(root);

        wait_for(
            || inner.upgrade().is_none(),
            Duration::from_secs(2),
            "active native port forward retained the stopped host runtime",
        );
        drop(forwarded);
        client.client.disconnect();
        server_thread.join().expect("managed server thread");
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
        let broadcaster = thread::spawn(move || run_broadcaster(broadcaster_inner));
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
