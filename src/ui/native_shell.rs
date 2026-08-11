//! The real native Task Cockpit shell entrypoint and its isolated host seam.
//!
//! This module deliberately owns only a local projection. It does not open the
//! installed profile, read the production session, start a legacy app, or
//! embed a WebView. A later inbox/host owner can provide a `ClientModel` and a
//! complete terminal model through the explicit seams below.

use std::cell::RefCell;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::env;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use gpui::{
    div, point, px, size, uniform_list, AnyElement, AppContext, Application, ClickEvent, Context,
    FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseUpEvent, ParentElement, Render, ScrollWheelEvent, StatefulInteractiveElement, Styled,
    Subscription, Task, Timer, UniformListScrollHandle, Window, WindowBounds, WindowOptions,
};
use gpui_component::button::{Button, ButtonVariants};

use crate::assets::AppAssets;
use crate::client::action;
use crate::client::UnsolicitedServerMessage;
use crate::client::{HostClient, HostClientConfig};
use crate::domain::id::TaskId;
use crate::domain::snapshot::SnapshotItem;
use crate::domain::task::TaskLifecycle;
use crate::domain::ClientId;
use crate::host::IpcError;
use crate::protocol::{Capability, CapabilitySet, FrameLimits};
use crate::ui::actions::{
    self, DockTool, HostActions, HostStatus, KeyboardAction, KeyboardModel, KeyboardShortcut,
    NativeDismissTransient, NativeDockArtifacts, NativeDockBrowser, NativeDockChanges,
    NativeDockFiles, NativeDockReview, NativeDockServices, NativeDockTerminal,
    NativeOpenCommandPalette, NativeOpenPalette, NativeOpenTaskSwitcher, NativeOpenTerminal,
    TaskCreate, TaskListAction, TaskRename, TaskShow,
};
use crate::ui::components::{
    AccessibilityMetadata, AccessibleRole, ActionEvent, ActionRequest, ActivationSource,
    FocusEpoch, FocusEpochSource, InteractionStateModel,
};
use crate::ui::shell::{
    NavigationResult, PointerButton, PointerOwner, Shell, TerminalPressRejection, TerminalRelease,
};
use crate::ui::task_cockpit::{
    TaskList, DEFAULT_VISIBLE_ROWS, FIXED_VIRTUAL_OVERSCAN, MAX_VIRTUAL_SOURCE_ROWS,
};
use crate::ui::terminal_adapter::TerminalDockAdapter;
pub use crate::ui::terminal_adapter::{TerminalDockState, TERMINAL_ADAPTER_DEPENDENCY};
use crate::ui::tokens::RuntimePreferencesSnapshot;

const NATIVE_PROFILE_DIR: &str = ".devmanager-next/dev-profile";
const NATIVE_PROFILE_NAME: &str = "native-next-dev";
const NATIVE_HOST_SCHEME: &str = "devtest";
const NATIVE_POINTER_ID: u64 = 1;
const MAX_RENDERED_TASK_ROWS: usize = DEFAULT_VISIBLE_ROWS + FIXED_VIRTUAL_OVERSCAN * 2;
const MAX_PENDING_HOST_ACTIONS: usize = 32;
const MAX_HOST_PROJECTIONS: usize = 64;
const MAX_HOST_SNAPSHOT_PAGES: usize = 512;
const CONTROLLER_TICK_INTERVAL: Duration = Duration::from_millis(16);

fn stable_task_element_id(task_id: TaskId) -> (&'static str, u64) {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in task_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    ("native-task-row", hash)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeShellError {
    WorkspaceMissing { path: PathBuf },
    WorkspaceMustBeRoot { path: PathBuf },
    ProfileOverride { value: String },
    WindowOpen { message: String },
    HostConnect { message: String },
    HeadlessRenderFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeHostState {
    Connected { endpoint: String },
    Disconnected,
    Error { message: String },
}

impl NativeHostState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Connected { .. } => "Connected",
            Self::Disconnected => "Disconnected",
            Self::Error { .. } => "Error",
        }
    }

    pub fn endpoint(&self) -> Option<&str> {
        match self {
            Self::Connected { endpoint } => Some(endpoint),
            Self::Disconnected | Self::Error { .. } => None,
        }
    }
}

impl Display for NativeShellError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorkspaceMissing { path } => {
                write!(formatter, "native shell workspace is not a directory: {}", path.display())
            }
            Self::WorkspaceMustBeRoot { path } => write!(
                formatter,
                "native shell expects a workspace root, not an existing profile path: {}",
                path.display()
            ),
            Self::ProfileOverride { value } => write!(
                formatter,
                "native shell refuses DEVMANAGER_PROFILE override `{value}`; use the generated isolated dev/test profile"
            ),
            Self::WindowOpen { message } => write!(formatter, "native shell window failed: {message}"),
            Self::HostConnect { message } => {
                write!(formatter, "native shell host connection failed: {message}")
            }
            Self::HeadlessRenderFailed => write!(formatter, "native shell headless render did not construct a root"),
        }
    }
}

impl Error for NativeShellError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IsolatedDevProfile {
    workspace_root: PathBuf,
    root: PathBuf,
}

impl IsolatedDevProfile {
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Config base to pass to the isolated `devmanager-host` process. The
    /// host's named profile is created beneath this generated directory; it
    /// never resolves the installed app-data root.
    pub fn host_config_base(&self) -> &Path {
        &self.root
    }

    /// The only profile name accepted by the development host attachment.
    ///
    /// The name is intentionally explicit and stable; callers must still
    /// supply the generated isolated profile root when launching the host.
    pub fn named_profile(&self) -> &'static str {
        NATIVE_PROFILE_NAME
    }

    /// Build the one client configuration used by the native-next shell.
    ///
    /// This does not connect, read a profile, or create files. A caller-owned
    /// host controller may use it from its I/O lane and then attach the single
    /// resulting [`HostClient`] through [`NativeHostClientRuntime`].
    pub fn host_client_config(&self) -> HostClientConfig {
        HostClientConfig {
            named_profile: self.named_profile().to_string(),
            client_build: format!("devmanager-next/{}", env!("CARGO_PKG_VERSION")),
            client_id: ClientId::new(),
            requested: CapabilitySet::from_capabilities([
                Capability::PagedSnapshots,
                Capability::EventReplay,
            ]),
            limits: FrameLimits::v1_default(),
        }
    }

    pub fn host_connection(&self) -> DevTestHostConnection {
        DevTestHostConnection {
            profile_root: self.root.clone(),
            endpoint: format!("{NATIVE_HOST_SCHEME}://{}", self.root.display()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DevTestHostConnection {
    profile_root: PathBuf,
    endpoint: String,
}

impl DevTestHostConnection {
    pub fn profile_root(&self) -> &Path {
        &self.profile_root
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

/// Exact command line used by the default native-next binary to own the
/// development host. Keeping this as a value object makes the process seam
/// injectable without allowing tests to start an installed host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeHostLaunchSpec {
    pub executable: PathBuf,
    pub profile: String,
    pub instance_label: String,
    pub parent_pid: u32,
    pub config_base: PathBuf,
}

impl NativeHostLaunchSpec {
    pub fn for_profile(
        profile: &IsolatedDevProfile,
        parent_pid: u32,
    ) -> Result<Self, NativeShellError> {
        if parent_pid == 0 {
            return Err(NativeShellError::HostConnect {
                message: "native host parent PID must be nonzero".to_string(),
            });
        }
        Ok(Self {
            executable: native_host_executable(),
            profile: profile.named_profile().to_string(),
            instance_label: "Native Next".to_string(),
            parent_pid,
            config_base: profile.host_config_base().to_path_buf(),
        })
    }

    pub fn arguments(&self) -> Vec<String> {
        vec![
            "--profile".to_string(),
            self.profile.clone(),
            "--instance-label".to_string(),
            self.instance_label.clone(),
            "--parent-pid".to_string(),
            self.parent_pid.to_string(),
            "--foreground".to_string(),
            "--config-base".to_string(),
            self.config_base.display().to_string(),
        ]
    }
}

fn native_host_executable() -> PathBuf {
    if let Ok(current) = std::env::current_exe() {
        let sibling = current
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(if cfg!(windows) {
                "devmanager-host.exe"
            } else {
                "devmanager-host"
            });
        if sibling.exists() {
            return sibling;
        }
    }
    PathBuf::from(if cfg!(windows) {
        "devmanager-host.exe"
    } else {
        "devmanager-host"
    })
}

enum NativeHostProcessKind {
    Child(Child),
}

/// Owns the one host child for the native shell. Drop always joins the child
/// after requesting termination, so no detached host survives a shell close.
pub struct NativeHostProcess {
    kind: NativeHostProcessKind,
}

impl std::fmt::Debug for NativeHostProcess {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeHostProcess")
            .field(
                "kind",
                &match self.kind {
                    NativeHostProcessKind::Child(_) => "child",
                },
            )
            .finish()
    }
}

impl Drop for NativeHostProcess {
    fn drop(&mut self) {
        let NativeHostProcessKind::Child(child) = &mut self.kind;
        let _ = child.kill();
        let _ = child.wait();
    }
}

pub trait NativeHostBootstrap {
    fn start(
        &mut self,
        profile: &IsolatedDevProfile,
    ) -> Result<NativeHostRuntimeAttachment, NativeShellError>;
}

#[derive(Debug, Default)]
struct ProcessNativeHostBootstrap;

impl NativeHostBootstrap for ProcessNativeHostBootstrap {
    fn start(
        &mut self,
        profile: &IsolatedDevProfile,
    ) -> Result<NativeHostRuntimeAttachment, NativeShellError> {
        ensure_isolated_host_config_base(profile)?;
        let spec = NativeHostLaunchSpec::for_profile(profile, std::process::id())?;
        let mut command = Command::new(&spec.executable);
        command.args(spec.arguments());
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command
            .spawn()
            .map_err(|error| NativeShellError::HostConnect {
                message: format!("isolated devmanager-host launch failed: {error}"),
            })?;
        let process = NativeHostProcess {
            kind: NativeHostProcessKind::Child(child),
        };
        let runtime = NativeHostClientRuntime::connect_blocking_with_process(profile, process)?;
        Ok(NativeHostRuntimeAttachment::Client(runtime))
    }
}

fn ensure_isolated_host_config_base(profile: &IsolatedDevProfile) -> Result<(), NativeShellError> {
    let config_base = profile.host_config_base();
    let workspace_root = profile.workspace_root();
    if let Some(parent) = config_base.parent() {
        if parent.exists() {
            let canonical_parent =
                std::fs::canonicalize(parent).map_err(|error| NativeShellError::HostConnect {
                    message: format!(
                        "isolated native host parent cannot be resolved {}: {error}",
                        parent.display()
                    ),
                })?;
            if !canonical_parent.starts_with(workspace_root) {
                return Err(NativeShellError::HostConnect {
                    message: format!(
                        "isolated native host parent escaped workspace: {}",
                        canonical_parent.display()
                    ),
                });
            }
        }
    }
    std::fs::create_dir_all(config_base).map_err(|error| NativeShellError::HostConnect {
        message: format!(
            "isolated native host config base could not be created {}: {error}",
            config_base.display()
        ),
    })?;
    let canonical_base =
        std::fs::canonicalize(config_base).map_err(|error| NativeShellError::HostConnect {
            message: format!(
                "isolated native host config base cannot be resolved {}: {error}",
                config_base.display()
            ),
        })?;
    if !canonical_base.starts_with(workspace_root) || canonical_base == workspace_root {
        return Err(NativeShellError::HostConnect {
            message: format!(
                "isolated native host config base escaped workspace: {}",
                canonical_base.display()
            ),
        });
    }
    Ok(())
}

/// Resolve only the generated profile under the caller's workspace. No
/// directory or config/session file is created by this function.
pub fn isolated_dev_profile(
    workspace_root: impl AsRef<Path>,
) -> Result<IsolatedDevProfile, NativeShellError> {
    let requested = workspace_root.as_ref();
    if path_is_profile_root(requested) {
        return Err(NativeShellError::WorkspaceMustBeRoot {
            path: requested.to_path_buf(),
        });
    }
    let workspace_root =
        std::fs::canonicalize(requested).map_err(|_| NativeShellError::WorkspaceMissing {
            path: requested.to_path_buf(),
        })?;
    if !workspace_root.is_dir() {
        return Err(NativeShellError::WorkspaceMissing {
            path: workspace_root,
        });
    }
    if let Some(value) = env::var_os("DEVMANAGER_PROFILE") {
        return Err(NativeShellError::ProfileOverride {
            value: value.to_string_lossy().into_owned(),
        });
    }
    Ok(IsolatedDevProfile {
        root: workspace_root.join(NATIVE_PROFILE_DIR),
        workspace_root,
    })
}

fn path_is_profile_root(path: &Path) -> bool {
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .map(|component| component.to_ascii_lowercase())
        .collect::<Vec<_>>();
    components
        .windows(2)
        .any(|pair| pair == [".devmanager-next", "dev-profile"])
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandlerTrace {
    pub focus_epoch: FocusEpoch,
    pub task_id: Option<TaskId>,
    pub request_generation: u64,
    pub consumed: bool,
    pub propagation_stopped: bool,
}

#[derive(Debug)]
pub struct NavigationHandlerOutcome {
    pub focus_epoch: FocusEpoch,
    pub task_id: TaskId,
    pub request_generation: u64,
    pub consumed: bool,
    pub propagation_stopped: bool,
    pub navigation: NavigationResult,
}

#[derive(Debug)]
pub struct TerminalHandlerOutcome {
    pub focus_epoch: FocusEpoch,
    pub task_id: TaskId,
    pub request_generation: u64,
    pub consumed: bool,
    pub propagation_stopped: bool,
    pub capture: Result<(), TerminalPressRejection>,
}

#[derive(Debug)]
pub struct TerminalReleaseOutcome {
    pub focus_epoch: FocusEpoch,
    pub task_id: Option<TaskId>,
    pub request_generation: u64,
    pub consumed: bool,
    pub propagation_stopped: bool,
    pub release: TerminalRelease,
}

#[derive(Clone, Debug)]
pub struct NativeActionRecord {
    pub id: &'static str,
    pub focus_epoch: FocusEpoch,
    pub request_generation: u64,
    pub event: ActionEvent,
    pub command: Option<NativeHostCommand>,
}

#[derive(Clone, Debug)]
pub enum NativeHostCommand {
    Envelope(crate::domain::command::CommandEnvelope),
    TaskCreate(crate::client::action::TaskCreateArguments),
    TaskRename {
        arguments: crate::client::action::TaskRenameArguments,
        expected_task_revision: u64,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativeKeyboardState {
    pub palette_open: bool,
    pub task_switcher_open: bool,
    pub command_palette_open: bool,
    pub selected_dock: Option<DockTool>,
    pub terminal_open: bool,
}

/// Result of handing one already-validated UI action to the host lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeHostActionResult {
    Queued,
    Disconnected,
    QueueFull,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeHostProjectionKind {
    Snapshot,
    Replay,
    Live,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeHostProjection {
    pub kind: NativeHostProjectionKind,
    pub task_list: Option<TaskList>,
    pub error: Option<String>,
}

enum NativeHostWorkerCommand {
    Execute(NativeActionRecord),
    Shutdown,
}

impl NativeHostProjection {
    pub fn kind(kind: NativeHostProjectionKind) -> Self {
        Self {
            kind,
            task_list: None,
            error: None,
        }
    }

    pub fn task_list(task_list: TaskList) -> Self {
        Self {
            kind: NativeHostProjectionKind::Snapshot,
            task_list: Some(task_list),
            error: None,
        }
    }
}

/// The sole native-next transport/action owner.
///
/// GPUI paint and event callbacks never await [`HostClient`]. They enqueue a
/// bounded, typed [`NativeActionRecord`] here; the caller's controller/task
/// lane drains the records and uses the same client for command/query I/O.
/// Keeping the client in this one owner prevents header, inbox, and shell
/// attachments from silently opening a second connection.
pub struct NativeHostClientRuntime {
    endpoint: String,
    client: Arc<Mutex<HostClient>>,
    pending: VecDeque<NativeActionRecord>,
    ready_projections: Arc<Mutex<VecDeque<NativeHostProjection>>>,
    command_tx: SyncSender<NativeHostWorkerCommand>,
    worker: Option<JoinHandle<()>>,
    runtime_guard: Option<Arc<tokio::runtime::Runtime>>,
    host_process: Option<NativeHostProcess>,
}

/// Injectable runtime seam used by deterministic shell tests. Production uses
/// [`NativeHostClientRuntime`] as the only concrete transport owner; tests can
/// supply this port without opening a named pipe or starting another client.
pub trait NativeHostRuntimePort: Send {
    fn endpoint(&self) -> &str;
    fn host_state(&self) -> NativeHostState;
    fn enqueue(&mut self, action: NativeActionRecord) -> NativeHostActionResult;
    fn drain_ready(&mut self, max: usize) -> Vec<NativeHostProjectionKind>;
    fn drain_projection_messages(&mut self, max: usize) -> Vec<NativeHostProjection> {
        self.drain_ready(max)
            .into_iter()
            .map(NativeHostProjection::kind)
            .collect()
    }
    fn take_pending(&mut self, max: usize) -> Vec<NativeActionRecord>;
    fn dispatch_pending(&mut self, action: NativeActionRecord) -> NativeHostActionResult;
    fn executed_count(&self) -> usize {
        0
    }
}

#[derive(Debug)]
pub struct NativeHostRuntimeStub {
    endpoint: String,
    state: NativeHostState,
    pending: VecDeque<NativeActionRecord>,
    projections: VecDeque<NativeHostProjection>,
    executed: Arc<Mutex<Vec<NativeActionRecord>>>,
}

impl NativeHostRuntimeStub {
    pub fn new(endpoint: impl Into<String>, state: NativeHostState) -> Self {
        Self {
            endpoint: endpoint.into(),
            state,
            pending: VecDeque::new(),
            projections: VecDeque::new(),
            executed: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn push_projection(&mut self, projection: NativeHostProjectionKind) {
        self.push_projection_message(NativeHostProjection::kind(projection));
    }

    pub fn push_projection_message(&mut self, projection: NativeHostProjection) {
        if self.projections.len() < MAX_HOST_PROJECTIONS {
            self.projections.push_back(projection);
        }
    }

    pub fn handle(&self) -> NativeHostRuntimeStubHandle {
        NativeHostRuntimeStubHandle {
            executed: Arc::clone(&self.executed),
        }
    }
}

#[derive(Clone, Debug)]
pub struct NativeHostRuntimeStubHandle {
    executed: Arc<Mutex<Vec<NativeActionRecord>>>,
}

impl NativeHostRuntimeStubHandle {
    pub fn executed_count(&self) -> usize {
        self.executed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

impl NativeHostRuntimePort for NativeHostRuntimeStub {
    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn host_state(&self) -> NativeHostState {
        self.state.clone()
    }

    fn enqueue(&mut self, action: NativeActionRecord) -> NativeHostActionResult {
        if !matches!(self.state, NativeHostState::Connected { .. }) {
            return NativeHostActionResult::Disconnected;
        }
        if self.pending.len() >= MAX_PENDING_HOST_ACTIONS {
            return NativeHostActionResult::QueueFull;
        }
        self.pending.push_back(action);
        NativeHostActionResult::Queued
    }

    fn drain_ready(&mut self, max: usize) -> Vec<NativeHostProjectionKind> {
        let count = max
            .min(MAX_PENDING_HOST_ACTIONS)
            .min(self.projections.len());
        self.projections
            .drain(..count)
            .map(|projection| projection.kind)
            .collect()
    }

    fn drain_projection_messages(&mut self, max: usize) -> Vec<NativeHostProjection> {
        let count = max.min(MAX_HOST_PROJECTIONS).min(self.projections.len());
        self.projections.drain(..count).collect()
    }

    fn take_pending(&mut self, max: usize) -> Vec<NativeActionRecord> {
        let count = max.min(MAX_PENDING_HOST_ACTIONS).min(self.pending.len());
        self.pending.drain(..count).collect()
    }

    fn dispatch_pending(&mut self, action: NativeActionRecord) -> NativeHostActionResult {
        self.executed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(action);
        NativeHostActionResult::Queued
    }

    fn executed_count(&self) -> usize {
        self.executed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

impl std::fmt::Debug for NativeHostClientRuntime {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeHostClientRuntime")
            .field(
                "connected",
                &self
                    .client
                    .lock()
                    .map(|client| client.is_connected())
                    .unwrap_or(false),
            )
            .field("pending_count", &self.pending.len())
            .field(
                "ready_projection_count",
                &self
                    .ready_projections
                    .lock()
                    .map(|projections| projections.len())
                    .unwrap_or_default(),
            )
            .field("runtime_guard", &self.runtime_guard.is_some())
            .field("worker", &self.worker.is_some())
            .finish()
    }
}

impl NativeHostClientRuntime {
    /// Connect once to the explicitly named isolated development host.
    ///
    /// The caller is responsible for launching that host with this profile's
    /// isolated config base. No production/default profile lookup is performed
    /// and no second connection is created by this type.
    pub async fn connect(profile: &IsolatedDevProfile) -> Result<Self, NativeShellError> {
        HostClient::connect(profile.host_client_config())
            .await
            .map(Self::new)
            .map_err(|error| NativeShellError::HostConnect {
                message: error.to_string(),
            })
    }

    /// Synchronous bootstrap for the default binary. The multi-thread Tokio
    /// runtime remains owned by this one client owner so the connection's
    /// reader/writer tasks continue draining while GPUI paints and handles
    /// input. A failed attempt becomes a typed shell error at the call site.
    pub fn connect_blocking(profile: &IsolatedDevProfile) -> Result<Self, NativeShellError> {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|error| NativeShellError::HostConnect {
                    message: format!("runtime bootstrap failed: {error}"),
                })?,
        );
        let client = runtime.block_on(HostClient::connect(profile.host_client_config()));
        let client = client.map_err(|error| NativeShellError::HostConnect {
            message: error.to_string(),
        })?;
        let mut runtime_owner = Self::new_with_runtime(client, runtime.clone());
        runtime
            .block_on(runtime_owner.bootstrap_projection())
            .map_err(|error| NativeShellError::HostConnect {
                message: error.to_string(),
            })?;
        Ok(runtime_owner)
    }

    fn connect_blocking_with_process(
        profile: &IsolatedDevProfile,
        process: NativeHostProcess,
    ) -> Result<Self, NativeShellError> {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|error| NativeShellError::HostConnect {
                    message: format!("runtime bootstrap failed: {error}"),
                })?,
        );
        let client = runtime.block_on(connect_with_startup_retry(profile));
        let client = client.map_err(|error| NativeShellError::HostConnect {
            message: error.to_string(),
        })?;
        let mut runtime_owner =
            Self::new_with_runtime_and_process(client, runtime.clone(), process);
        runtime
            .block_on(runtime_owner.bootstrap_projection())
            .map_err(|error| NativeShellError::HostConnect {
                message: error.to_string(),
            })?;
        Ok(runtime_owner)
    }

    pub fn new(client: HostClient) -> Self {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("native host runtime must be constructible"),
        );
        Self::new_with_runtime(client, runtime)
    }

    fn new_with_runtime(client: HostClient, runtime: Arc<tokio::runtime::Runtime>) -> Self {
        Self::new_with_runtime_guard(client, Some(runtime))
    }

    fn new_with_runtime_guard(
        client: HostClient,
        runtime_guard: Option<Arc<tokio::runtime::Runtime>>,
    ) -> Self {
        let endpoint = client.endpoint().to_string();
        let client = Arc::new(Mutex::new(client));
        let (command_tx, command_rx) = std::sync::mpsc::sync_channel(MAX_PENDING_HOST_ACTIONS);
        let client_for_worker = Arc::clone(&client);
        let runtime_for_worker = runtime_guard.clone();
        let projections_for_worker = Arc::new(Mutex::new(VecDeque::new()));
        let projections_for_worker_thread = Arc::clone(&projections_for_worker);
        let worker = std::thread::Builder::new()
            .name("devmanager-native-host-worker".to_string())
            .spawn(move || {
                native_host_worker_loop(
                    client_for_worker,
                    runtime_for_worker,
                    command_rx,
                    projections_for_worker_thread,
                )
            })
            .ok();
        Self {
            endpoint,
            client,
            pending: VecDeque::new(),
            ready_projections: projections_for_worker,
            command_tx,
            worker,
            runtime_guard,
            host_process: None,
        }
    }

    fn new_with_runtime_and_process(
        client: HostClient,
        runtime: Arc<tokio::runtime::Runtime>,
        process: NativeHostProcess,
    ) -> Self {
        let mut runtime_owner = Self::new_with_runtime_guard(client, Some(runtime));
        runtime_owner.host_process = Some(process);
        runtime_owner
    }

    pub fn is_connected(&self) -> bool {
        self.client
            .lock()
            .map(|client| client.is_connected())
            .unwrap_or(false)
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Queue one action without performing transport work on the UI thread.
    pub fn enqueue(&mut self, action: NativeActionRecord) -> NativeHostActionResult {
        if !self.is_connected() {
            return NativeHostActionResult::Disconnected;
        }
        if self.pending.len() >= MAX_PENDING_HOST_ACTIONS {
            return NativeHostActionResult::QueueFull;
        }
        self.pending.push_back(action);
        NativeHostActionResult::Queued
    }

    /// Drain a bounded batch for the controller/task lane.
    pub fn take_pending(&mut self) -> Vec<NativeActionRecord> {
        self.take_pending_bounded(MAX_PENDING_HOST_ACTIONS)
    }

    pub fn take_pending_bounded(&mut self, max: usize) -> Vec<NativeActionRecord> {
        let count = max.min(MAX_PENDING_HOST_ACTIONS).min(self.pending.len());
        self.pending.drain(..count).collect()
    }

    pub fn dispatch_pending(&mut self, action: NativeActionRecord) -> NativeHostActionResult {
        self.command_tx
            .try_send(NativeHostWorkerCommand::Execute(action))
            .map(|_| NativeHostActionResult::Queued)
            .unwrap_or(NativeHostActionResult::QueueFull)
    }

    /// Drain only already-buffered unsolicited host projections. This method
    /// is intended for a controller/task lane; paint and input callbacks never
    /// call it. The zero-duration timeout makes the live lane nonblocking.
    pub async fn drain_bounded(
        &mut self,
        max: usize,
    ) -> Result<Vec<NativeHostProjectionKind>, IpcError> {
        Ok(self.take_ready_projections(max))
    }

    pub fn take_ready_projections(&mut self, max: usize) -> Vec<NativeHostProjectionKind> {
        self.ready_projections
            .lock()
            .map(|mut projections| {
                let count = max.min(MAX_PENDING_HOST_ACTIONS).min(projections.len());
                projections
                    .drain(..count)
                    .map(|projection| projection.kind)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn take_ready_projection_messages(&mut self, max: usize) -> Vec<NativeHostProjection> {
        self.ready_projections
            .lock()
            .map(|mut projections| {
                let count = max.min(MAX_HOST_PROJECTIONS).min(projections.len());
                projections.drain(..count).collect()
            })
            .unwrap_or_default()
    }

    /// Perform the initial bounded paged snapshot and durable replay handoff
    /// on the controller lane. The worker retains only stable task IDs and
    /// sends one immutable projection to GPUI; row elements are still created
    /// solely by the uniform-list viewport.
    pub async fn bootstrap_projection(
        &mut self,
    ) -> Result<Vec<NativeHostProjectionKind>, NativeShellError> {
        let mut client = self
            .client
            .lock()
            .map_err(|_| NativeShellError::HostConnect {
                message: "native host client lock poisoned".to_string(),
            })?;
        let mut projection = Vec::with_capacity(2);
        let mut snapshot_id = None;
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        let mut task_ids = Vec::new();
        let mut pages_read = 0;
        loop {
            pages_read += 1;
            if pages_read > MAX_HOST_SNAPSHOT_PAGES {
                return Err(NativeShellError::HostConnect {
                    message: format!(
                        "native host task snapshot exceeded {} pages",
                        MAX_HOST_SNAPSHOT_PAGES
                    ),
                });
            }
            let page = match client
                .snapshot_page(
                    crate::domain::snapshot::SnapshotSection::Tasks,
                    snapshot_id,
                    cursor.clone(),
                )
                .await
                .map_err(|error| NativeShellError::HostConnect {
                    message: error.to_string(),
                })? {
                Ok(page) => page,
                Err(error) => {
                    return Err(NativeShellError::HostConnect {
                        message: format!("{error:?}"),
                    })
                }
            };
            snapshot_id = Some(page.snapshot_id);
            for item in page.items {
                if let SnapshotItem::Task(task) = item {
                    if task.task.lifecycle != TaskLifecycle::Archived {
                        task_ids.push(task.task.id);
                    }
                }
            }
            if task_ids.len() >= MAX_VIRTUAL_SOURCE_ROWS {
                task_ids.truncate(MAX_VIRTUAL_SOURCE_ROWS);
                break;
            }
            let Some(next_cursor) = page.next_cursor else {
                break;
            };
            if !seen_cursors.insert(next_cursor.clone()) {
                return Err(NativeShellError::HostConnect {
                    message: "native host task snapshot repeated a cursor".to_string(),
                });
            }
            cursor = Some(next_cursor);
        }
        let task_list = TaskList::from_virtual_task_ids(task_ids).map_err(|overflow| {
            NativeShellError::HostConnect {
                message: format!(
                    "native host task snapshot exceeded {} rows (observed {})",
                    overflow.limit, overflow.total_count
                ),
            }
        })?;
        let snapshot_id = snapshot_id.ok_or_else(|| NativeShellError::HostConnect {
            message: "native host task snapshot returned no snapshot identity".to_string(),
        })?;
        client
            .release_snapshot(snapshot_id)
            .await
            .map_err(|error| NativeShellError::HostConnect {
                message: error.to_string(),
            })?
            .map_err(|error| NativeShellError::HostConnect {
                message: format!("{error:?}"),
            })?;
        projection.push(NativeHostProjectionKind::Snapshot);
        let snapshot_projection = NativeHostProjection {
            kind: NativeHostProjectionKind::Snapshot,
            task_list: Some(task_list),
            error: None,
        };
        match client
            .open_event_replay(0)
            .await
            .map_err(|error| NativeShellError::HostConnect {
                message: error.to_string(),
            })? {
            Ok(_) => projection.push(NativeHostProjectionKind::Replay),
            Err(error) => {
                return Err(NativeShellError::HostConnect {
                    message: format!("{error:?}"),
                })
            }
        }
        if let Ok(mut projections) = self.ready_projections.lock() {
            if projections.len() < MAX_HOST_PROJECTIONS {
                projections.push_back(snapshot_projection);
            }
            let remaining = MAX_HOST_PROJECTIONS.saturating_sub(projections.len());
            projections.extend(
                projection
                    .iter()
                    .copied()
                    .filter(|kind| *kind != NativeHostProjectionKind::Snapshot)
                    .take(remaining)
                    .map(NativeHostProjection::kind),
            );
        }
        Ok(projection)
    }

    /// Execute a caller-created, revision-fenced command on this same client.
    ///
    /// The shell intentionally does not synthesize envelopes: task identity,
    /// expected revision, and command id belong to the host/controller lane.
    pub async fn execute_command(
        &mut self,
        envelope: crate::domain::command::CommandEnvelope,
    ) -> Result<crate::domain::command::CommandReceipt, IpcError> {
        let mut client = self.client.lock().map_err(|_| IpcError::Unavailable)?;
        client.execute_command(envelope).await
    }
}

impl NativeHostRuntimePort for NativeHostClientRuntime {
    fn endpoint(&self) -> &str {
        self.endpoint()
    }

    fn host_state(&self) -> NativeHostState {
        if self.is_connected() {
            NativeHostState::Connected {
                endpoint: self.endpoint().to_string(),
            }
        } else {
            NativeHostState::Disconnected
        }
    }

    fn enqueue(&mut self, action: NativeActionRecord) -> NativeHostActionResult {
        NativeHostClientRuntime::enqueue(self, action)
    }

    fn drain_ready(&mut self, max: usize) -> Vec<NativeHostProjectionKind> {
        self.take_ready_projections(max)
    }

    fn drain_projection_messages(&mut self, max: usize) -> Vec<NativeHostProjection> {
        self.take_ready_projection_messages(max)
    }

    fn take_pending(&mut self, max: usize) -> Vec<NativeActionRecord> {
        self.take_pending_bounded(max)
    }

    fn dispatch_pending(&mut self, action: NativeActionRecord) -> NativeHostActionResult {
        NativeHostClientRuntime::dispatch_pending(self, action)
    }
}

impl Drop for NativeHostClientRuntime {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.command_tx.send(NativeHostWorkerCommand::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn connect_with_startup_retry(profile: &IsolatedDevProfile) -> Result<HostClient, IpcError> {
    let config = profile.host_client_config();
    let mut last_error = None;
    for _ in 0..40 {
        match HostClient::connect(config.clone()).await {
            Ok(client) => return Ok(client),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
    Err(last_error.unwrap_or(IpcError::Unavailable))
}

fn native_host_worker_loop(
    client: Arc<Mutex<HostClient>>,
    runtime: Option<Arc<tokio::runtime::Runtime>>,
    command_rx: Receiver<NativeHostWorkerCommand>,
    projections: Arc<Mutex<VecDeque<NativeHostProjection>>>,
) {
    let Some(runtime) = runtime else {
        return;
    };
    loop {
        match command_rx.recv_timeout(CONTROLLER_TICK_INTERVAL) {
            Ok(NativeHostWorkerCommand::Shutdown) => break,
            Ok(NativeHostWorkerCommand::Execute(action)) => {
                if let Some(command) = action.command {
                    let result = client.lock().ok().map(|mut client| {
                        runtime.block_on(execute_native_command(&mut client, command))
                    });
                    let projection = match result {
                        Some(Ok(())) => NativeHostProjection::kind(NativeHostProjectionKind::Live),
                        Some(Err(error)) => NativeHostProjection {
                            kind: NativeHostProjectionKind::Error,
                            task_list: None,
                            error: Some(bounded_host_error(error.to_string())),
                        },
                        None => NativeHostProjection {
                            kind: NativeHostProjectionKind::Error,
                            task_list: None,
                            error: Some("native host client lock poisoned".to_string()),
                        },
                    };
                    if let Ok(mut queue) = projections.lock() {
                        if queue.len() < MAX_HOST_PROJECTIONS {
                            queue.push_back(projection);
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let message = client.lock().ok().and_then(|client| {
            runtime
                .block_on(tokio::time::timeout(
                    Duration::ZERO,
                    client.recv_unsolicited(),
                ))
                .ok()
                .and_then(Result::ok)
        });
        if let Some(message) = message {
            let kind = match message {
                UnsolicitedServerMessage::DurableEvent { .. }
                | UnsolicitedServerMessage::Stream(_) => NativeHostProjectionKind::Live,
                UnsolicitedServerMessage::ResyncRequired { .. } => NativeHostProjectionKind::Replay,
            };
            if let Ok(mut queue) = projections.lock() {
                if queue.len() < MAX_HOST_PROJECTIONS {
                    queue.push_back(NativeHostProjection::kind(kind));
                }
            }
        }
    }
}

async fn execute_native_command(
    client: &mut HostClient,
    command: NativeHostCommand,
) -> Result<(), IpcError> {
    let envelope = match command {
        NativeHostCommand::Envelope(envelope) => envelope,
        NativeHostCommand::TaskCreate(arguments) => crate::client::action::task_create_command(
            crate::domain::id::CommandId::new(),
            client.client_id(),
            unix_time_ms(),
            arguments,
        )
        .map_err(|_| IpcError::Unavailable)?,
        NativeHostCommand::TaskRename {
            arguments,
            expected_task_revision,
        } => crate::client::action::task_rename_command(
            crate::domain::id::CommandId::new(),
            client.client_id(),
            unix_time_ms(),
            expected_task_revision,
            arguments,
        )
        .map_err(|_| IpcError::Unavailable)?,
    };
    client.execute_command(envelope).await.map(|_| ())
}

fn unix_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

pub enum NativeHostRuntimeAttachment {
    Client(NativeHostClientRuntime),
    Injected(Box<dyn NativeHostRuntimePort>),
}

impl std::fmt::Debug for NativeHostRuntimeAttachment {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeHostRuntimeAttachment")
            .field(
                "kind",
                &match self {
                    Self::Client(_) => "client",
                    Self::Injected(_) => "injected",
                },
            )
            .finish()
    }
}

/// Pure handler state shared by the native GPUI callbacks and focused tests.
/// Every event captures focus epoch, task identity, and a monotonic request
/// generation before it can reach a host-facing action.
#[derive(Debug)]
pub struct NativeInteraction {
    shell: Shell,
    focus_epochs: FocusEpochSource,
    interaction: InteractionStateModel,
    request_generation: u64,
    pointer_owner: Option<PointerOwner>,
    last_handler: Option<HandlerTrace>,
    keyboard_state: NativeKeyboardState,
    pending_keyboard: Option<(FocusEpoch, u64, KeyboardAction)>,
}

impl NativeInteraction {
    pub fn new(selected_task: Option<TaskId>) -> Self {
        Self {
            shell: Shell::new(selected_task),
            focus_epochs: FocusEpochSource::new(),
            interaction: InteractionStateModel::default(),
            request_generation: 0,
            pointer_owner: None,
            last_handler: None,
            keyboard_state: NativeKeyboardState::default(),
            pending_keyboard: None,
        }
    }

    pub fn selected_task(&self) -> Option<TaskId> {
        self.shell.selected_task()
    }

    pub fn current_focus_epoch(&self) -> FocusEpoch {
        self.focus_epochs.current()
    }

    pub fn last_handler(&self) -> Option<&HandlerTrace> {
        self.last_handler.as_ref()
    }

    pub fn accepts_action_record(&self, record: &NativeActionRecord) -> bool {
        self.last_handler.as_ref().is_some_and(|trace| {
            trace.focus_epoch == record.focus_epoch
                && trace.request_generation == record.request_generation
                && trace.consumed
                && trace.propagation_stopped
        })
    }

    pub fn set_disabled(&mut self, disabled: bool) {
        self.interaction.set_disabled(disabled);
    }

    pub fn set_loading(
        &mut self,
        loading: bool,
    ) -> Result<(), crate::ui::components::ComponentError> {
        self.interaction.set_loading(loading)
    }

    pub fn interaction_state(&self) -> crate::ui::components::InteractionState {
        self.interaction.state()
    }

    pub fn sync_selected_task(&mut self, selected_task: Option<TaskId>) -> bool {
        self.shell.sync_selected_task(selected_task)
    }

    pub fn keyboard_state(&self) -> NativeKeyboardState {
        self.keyboard_state
    }

    fn begin_handler(&mut self, task_id: Option<TaskId>) -> (FocusEpoch, u64) {
        // Any pointer, terminal, or keyboard event advances the capture
        // generation. Invalidate a previously resolved key intent before the
        // new handler can mutate shell state.
        self.pending_keyboard = None;
        let focus_epoch = self.focus_epochs.current();
        self.interaction.set_focus_epoch(focus_epoch);
        let request_generation = self
            .request_generation
            .checked_add(1)
            .expect("native action request generation exhausted");
        self.request_generation = request_generation;
        self.focus_epochs.advance();
        self.last_handler = Some(HandlerTrace {
            focus_epoch,
            task_id,
            request_generation,
            consumed: true,
            propagation_stopped: true,
        });
        (focus_epoch, request_generation)
    }

    pub fn navigation_mouse_down(
        &mut self,
        task_id: TaskId,
        task_list: &TaskList,
    ) -> NavigationHandlerOutcome {
        let (focus_epoch, request_generation) = self.begin_handler(Some(task_id));
        let navigation =
            self.shell
                .navigation_mouse_down(task_id, self.shell.navigation_epoch(), task_list);
        NavigationHandlerOutcome {
            focus_epoch,
            task_id,
            request_generation,
            consumed: navigation.consumed(),
            propagation_stopped: true,
            navigation,
        }
    }

    pub fn terminal_mouse_down(
        &mut self,
        pointer_id: u64,
        task_id: TaskId,
        button: PointerButton,
        projected_selected_task: Option<TaskId>,
    ) -> TerminalHandlerOutcome {
        let (focus_epoch, request_generation) = self.begin_handler(Some(task_id));
        let capture = self.shell.terminal_mouse_down(
            pointer_id,
            task_id,
            button,
            self.shell.navigation_epoch(),
            projected_selected_task,
        );
        let capture = match capture {
            Ok(owner) => {
                self.pointer_owner = Some(owner);
                Ok(())
            }
            Err(error) => Err(error),
        };
        TerminalHandlerOutcome {
            focus_epoch,
            task_id,
            request_generation,
            consumed: true,
            propagation_stopped: true,
            capture,
        }
    }

    pub fn terminal_mouse_up(&mut self) -> TerminalReleaseOutcome {
        let task_id = self.selected_task();
        let (focus_epoch, request_generation) = self.begin_handler(task_id);
        let release = self.shell.terminal_mouse_up(self.pointer_owner.take());
        TerminalReleaseOutcome {
            focus_epoch,
            task_id,
            request_generation,
            consumed: release.consumed(),
            propagation_stopped: true,
            release,
        }
    }

    pub fn keyboard(
        &mut self,
        keyboard: &KeyboardModel,
        shortcut: KeyboardShortcut,
    ) -> Option<(FocusEpoch, u64, KeyboardAction)> {
        let (focus_epoch, request_generation) = self.begin_handler(self.selected_task());
        let result = keyboard
            .activate(shortcut, &self.interaction, focus_epoch)
            .map(|action| (focus_epoch, request_generation, action));
        self.pending_keyboard = result;
        result
    }

    /// Commit one resolved keyboard intent only if it is still the current
    /// focus/request capture. A later event invalidates the old tuple, which
    /// prevents stale key callbacks from mutating shell state.
    pub fn commit_keyboard_action(
        &mut self,
        focus_epoch: FocusEpoch,
        request_generation: u64,
        action: KeyboardAction,
    ) -> bool {
        if self.pending_keyboard != Some((focus_epoch, request_generation, action))
            || !self.interaction.state().can_activate()
        {
            return false;
        }
        match action {
            KeyboardAction::OpenPalette => {
                self.keyboard_state.palette_open = true;
                self.keyboard_state.task_switcher_open = false;
                self.keyboard_state.command_palette_open = false;
            }
            KeyboardAction::OpenTaskSwitcher => {
                self.keyboard_state.task_switcher_open = true;
                self.keyboard_state.palette_open = false;
                self.keyboard_state.command_palette_open = false;
            }
            KeyboardAction::OpenCommandPalette => {
                self.keyboard_state.command_palette_open = true;
                self.keyboard_state.palette_open = false;
                self.keyboard_state.task_switcher_open = false;
            }
            KeyboardAction::SelectDock(tool) => self.keyboard_state.selected_dock = Some(tool),
            KeyboardAction::OpenTerminal => self.keyboard_state.terminal_open = true,
            KeyboardAction::DismissTransient => {
                self.keyboard_state = NativeKeyboardState::default();
            }
        }
        self.pending_keyboard = None;
        true
    }

    pub fn action(&mut self, request: ActionRequest) -> Option<NativeActionRecord> {
        self.action_from_source(
            request,
            ActivationSource::Keyboard {
                key: crate::ui::components::KeyboardKey::Enter,
            },
        )
    }

    pub fn action_from_source(
        &mut self,
        request: ActionRequest,
        source: ActivationSource,
    ) -> Option<NativeActionRecord> {
        let descriptor = action::catalog()
            .iter()
            .find(|descriptor| descriptor.id == request.id())?;
        if !self.interaction.state().can_activate() {
            return None;
        }
        let selected_task = self.selected_task();
        let request_task = match &request {
            ActionRequest::TaskShow { task_id } => Some(*task_id),
            ActionRequest::TaskRename(arguments) => Some(arguments.task_id),
            _ => None,
        };
        if request_task.is_some() && request_task != selected_task {
            return None;
        }
        let (focus_epoch, request_generation) = self.begin_handler(self.selected_task());
        let command = match &request {
            ActionRequest::TaskCreate(arguments) => {
                Some(NativeHostCommand::TaskCreate(arguments.clone()))
            }
            ActionRequest::TaskRename(arguments) => Some(NativeHostCommand::TaskRename {
                arguments: arguments.clone(),
                expected_task_revision: 0,
            }),
            _ => None,
        };
        let event = ActionEvent::new(request, source, focus_epoch);
        Some(NativeActionRecord {
            id: descriptor.id,
            focus_epoch,
            request_generation,
            event,
            command,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessibilityNode {
    metadata: AccessibilityMetadata,
    element_id: String,
    focusable: bool,
    tab_stop: bool,
    children: Vec<AccessibilityNode>,
}

impl AccessibilityNode {
    fn new(role: AccessibleRole, name: impl Into<String>, description: impl Into<String>) -> Self {
        let mut metadata = AccessibilityMetadata::new(role, name)
            .expect("native shell semantic names are bounded literals");
        metadata
            .set_description(description)
            .expect("native shell semantic descriptions are bounded literals");
        Self {
            metadata,
            element_id: String::new(),
            focusable: false,
            tab_stop: false,
            children: Vec::new(),
        }
    }

    fn gpui(mut self, element_id: impl Into<String>, focusable: bool, tab_stop: bool) -> Self {
        self.element_id = element_id.into();
        self.focusable = focusable;
        self.tab_stop = tab_stop;
        self
    }

    fn with_children(mut self, children: Vec<AccessibilityNode>) -> Self {
        self.children = children;
        self
    }

    pub fn name(&self) -> &str {
        &self.metadata.name
    }

    pub fn description(&self) -> &str {
        &self.metadata.description
    }

    pub fn role(&self) -> AccessibleRole {
        self.metadata.role
    }

    pub fn metadata(&self) -> &AccessibilityMetadata {
        &self.metadata
    }

    pub fn children(&self) -> &[AccessibilityNode] {
        &self.children
    }

    pub fn element_id(&self) -> &str {
        &self.element_id
    }

    pub fn focusable(&self) -> bool {
        self.focusable
    }

    pub fn tab_stop(&self) -> bool {
        self.tab_stop
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeAccessibilityNode {
    pub element_id: String,
    pub role: AccessibleRole,
    pub label: String,
    pub description: String,
    pub focusable: bool,
    pub tab_stop: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessibilityTree {
    root: AccessibilityNode,
    rendered_task_count: usize,
}

impl AccessibilityTree {
    pub fn for_task_list(task_list: &TaskList, selected_task: Option<TaskId>) -> Self {
        let rows = task_list
            .rendered_task_ids()
            .iter()
            .map(|task_id| {
                let mut row = AccessibilityNode::new(
                    AccessibleRole::Button,
                    format!("Task {task_id}"),
                    "Select this task and open its native task cockpit.",
                )
                .gpui(format!("native-task-row-{}", task_id), true, true);
                row.metadata.focused = selected_task == Some(*task_id);
                row
            })
            .collect::<Vec<_>>();
        let inbox_status = if rows.is_empty() {
            AccessibilityNode::new(
                AccessibleRole::Status,
                "No tasks in isolated inbox",
                "The dev/test host has not supplied a task snapshot.",
            )
            .gpui("native-task-inbox-status", false, false)
        } else {
            AccessibilityNode::new(
                AccessibleRole::Status,
                "Task inbox ready",
                "Only the bounded visible task window is rendered.",
            )
            .gpui("native-task-inbox-status", false, false)
        };
        let inbox = AccessibilityNode::new(
            AccessibleRole::Region,
            "Task inbox",
            "Bounded virtualized task list; keyboard and pointer navigation share one focus epoch.",
        )
        .gpui("native-task-inbox", false, false)
        .with_children(
            std::iter::once(inbox_status)
                .chain(rows)
                .collect::<Vec<_>>(),
        );
        let toolbar = AccessibilityNode::new(
            AccessibleRole::Region,
            "Task cockpit actions",
            "Actions are dispatched through the shared client action catalog.",
        )
        .gpui("native-shell-toolbar", false, false);
        let terminal = AccessibilityNode::new(
            AccessibleRole::Status,
            "Terminal dock",
            TerminalDockState::unavailable().message(),
        )
        .gpui("native-shell-terminal-dock", false, false);
        let root = AccessibilityNode::new(
            AccessibleRole::Region,
            "Task Cockpit",
            "Native GPUI shell using an isolated dev/test host profile.",
        )
        .gpui("native-shell-root", true, true)
        .with_children(vec![toolbar, inbox, terminal]);
        Self {
            root,
            rendered_task_count: task_list.rendered_task_ids().len(),
        }
    }

    pub fn root(&self) -> &AccessibilityNode {
        &self.root
    }

    pub fn rendered_task_count(&self) -> usize {
        self.rendered_task_count
    }

    pub fn nodes(&self) -> Vec<&AccessibilityNode> {
        fn visit<'a>(node: &'a AccessibilityNode, output: &mut Vec<&'a AccessibilityNode>) {
            output.push(node);
            for child in &node.children {
                visit(child, output);
            }
        }
        let mut nodes = Vec::new();
        visit(&self.root, &mut nodes);
        nodes
    }

    /// Return the platform update produced from this same semantic tree. This
    /// is used by inspectable accessibility tests and by the Windows bridge;
    /// it is not a second metadata model.
    pub fn platform_update_for_test(&self) -> accesskit::TreeUpdate {
        accesskit_tree_update(self)
    }

    /// The GPUI 0.2.2 element API exposes stable IDs, labels, and focus/tab
    /// hooks rather than a platform accessibility namespace. This projection
    /// is built from those exact IDs and hooks and is used by headless tests to
    /// inspect the rendered control/tree semantics.
    pub fn gpui_nodes(&self) -> Vec<NativeAccessibilityNode> {
        self.nodes()
            .into_iter()
            .map(|node| NativeAccessibilityNode {
                element_id: node.element_id.clone(),
                role: node.role(),
                label: node.name().to_string(),
                description: node.description().to_string(),
                focusable: node.focusable,
                tab_stop: node.tab_stop,
            })
            .collect()
    }
}

struct NativePlatformAccessibilityBridge {
    tree: Arc<Mutex<accesskit::TreeUpdate>>,
    node_count: usize,
    attached: bool,
    #[cfg(windows)]
    adapter: Option<accesskit_windows::SubclassingAdapter>,
}

impl NativePlatformAccessibilityBridge {
    fn new(tree: &AccessibilityTree) -> Self {
        let update = accesskit_tree_update(tree);
        Self {
            tree: Arc::new(Mutex::new(update)),
            node_count: tree.nodes().len(),
            attached: false,
            #[cfg(windows)]
            adapter: None,
        }
    }

    fn is_available(&self) -> bool {
        true
    }

    fn node_count(&self) -> usize {
        self.node_count
    }

    fn tree_update(&self) -> accesskit::TreeUpdate {
        self.tree
            .lock()
            .map(|tree| tree.clone())
            .unwrap_or_else(|_| {
                accesskit_tree_update(&AccessibilityTree::for_task_list(&TaskList::empty(), None))
            })
    }

    fn attach_window(&mut self, window: &Window) {
        #[cfg(windows)]
        {
            use accesskit_windows::HWND;
            use raw_window_handle::RawWindowHandle;
            let Ok(handle) = raw_window_handle::HasWindowHandle::window_handle(window) else {
                return;
            };
            let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                return;
            };
            let hwnd = HWND(handle.hwnd.get() as *mut std::ffi::c_void);
            let activation = NativeAccessKitActivation {
                tree: Arc::clone(&self.tree),
            };
            self.adapter = Some(accesskit_windows::SubclassingAdapter::new(
                hwnd,
                activation,
                NativeAccessKitActionHandler,
            ));
            self.attached = true;
        }
        #[cfg(not(windows))]
        let _ = window;
    }

    fn sync(&mut self, tree: &AccessibilityTree) {
        self.node_count = tree.nodes().len();
        let update = accesskit_tree_update(tree);
        if let Ok(mut current) = self.tree.lock() {
            *current = update.clone();
        }
        #[cfg(windows)]
        if let Some(adapter) = self.adapter.as_mut() {
            if let Some(events) = adapter.update_if_active(|| update) {
                events.raise();
            }
        }
    }
}

fn accesskit_tree_update(tree: &AccessibilityTree) -> accesskit::TreeUpdate {
    use accesskit::{Node, NodeId, Role, Tree, TreeId, TreeUpdate};
    fn role(role: AccessibleRole) -> Role {
        match role {
            AccessibleRole::Button => Role::Button,
            AccessibleRole::TextField => Role::TextInput,
            AccessibleRole::Status => Role::Status,
            AccessibleRole::Alert => Role::Alert,
            AccessibleRole::Region => Role::Region,
        }
    }
    fn visit(
        source: &AccessibilityNode,
        nodes: &mut Vec<(NodeId, Node)>,
        next: &mut u64,
        focused: &mut Option<NodeId>,
    ) -> NodeId {
        let id = NodeId::from(*next);
        *next += 1;
        let children = source
            .children()
            .iter()
            .map(|child| visit(child, nodes, next, focused))
            .collect::<Vec<_>>();
        let mut node = Node::new(role(source.role()));
        node.set_label(source.name().to_string());
        node.set_description(source.description().to_string());
        node.set_author_id(source.element_id().to_string());
        if source.metadata().focused {
            node.set_selected(true);
            *focused = Some(id);
        }
        node.set_children(children);
        nodes.push((id, node));
        id
    }
    let mut nodes = Vec::new();
    let mut next = 0;
    let mut focused = None;
    let root = visit(tree.root(), &mut nodes, &mut next, &mut focused);
    TreeUpdate {
        nodes,
        tree: Some(Tree::new(root)),
        tree_id: TreeId::ROOT,
        focus: focused.unwrap_or(root),
    }
}

#[cfg(windows)]
struct NativeAccessKitActivation {
    tree: Arc<Mutex<accesskit::TreeUpdate>>,
}

#[cfg(windows)]
impl accesskit::ActivationHandler for NativeAccessKitActivation {
    fn request_initial_tree(&mut self) -> Option<accesskit::TreeUpdate> {
        self.tree.lock().ok().map(|tree| tree.clone())
    }
}

#[cfg(windows)]
struct NativeAccessKitActionHandler;

#[cfg(windows)]
impl accesskit::ActionHandler for NativeAccessKitActionHandler {
    fn do_action(&mut self, _request: accesskit::ActionRequest) {}
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeRenderSmokeReport {
    pub root_constructed: bool,
    pub semantic_nodes: usize,
    pub rendered_task_rows: usize,
    pub host_profile: PathBuf,
    pub profile_root: PathBuf,
    pub host_state: NativeHostState,
    pub gpui_accessibility_nodes: Vec<NativeAccessibilityNode>,
    pub platform_accessibility_bridge: bool,
    pub platform_accessibility_nodes: usize,
    pub platform_accessibility_roles: Vec<accesskit::Role>,
    pub platform_accessibility_focus_is_root: bool,
}

/// Build a real GPUI element tree without opening a window or touching a host.
/// This is used by focused acceptance tests and is intentionally distinct from
/// the host-free preview CLI.
pub fn headless_render_smoke(
    workspace_root: impl AsRef<Path>,
) -> Result<NativeRenderSmokeReport, NativeShellError> {
    let profile = isolated_dev_profile(workspace_root)?;
    let profile_root = profile.root().to_path_buf();
    let report_slot = Rc::new(RefCell::new(None));
    let report_slot_for_app = Rc::clone(&report_slot);
    Application::headless()
        .with_assets(AppAssets::new())
        .run(move |cx| {
            crate::ui::init(cx);
            let entity = cx.new(|cx| NativeShell::new_for_headless(profile.clone(), cx));
            let report = entity.update(cx, |shell, _cx| {
                let _root = shell.element_without_handlers();
                let platform_tree = shell.platform_accessibility_tree_for_test();
                NativeRenderSmokeReport {
                    root_constructed: true,
                    semantic_nodes: shell.accessibility_tree.nodes().len(),
                    rendered_task_rows: shell.rendered_task_count(),
                    host_profile: shell.host_connection.profile_root().to_path_buf(),
                    profile_root: shell.profile.root().to_path_buf(),
                    host_state: shell.host_state.clone(),
                    gpui_accessibility_nodes: shell.accessibility_tree.gpui_nodes(),
                    platform_accessibility_bridge: shell.platform_accessibility.is_available(),
                    platform_accessibility_nodes: shell.platform_accessibility.node_count(),
                    platform_accessibility_roles: platform_tree
                        .nodes
                        .iter()
                        .map(|(_, node)| node.role())
                        .collect(),
                    platform_accessibility_focus_is_root: platform_tree.focus
                        == accesskit::NodeId::from(0),
                }
            });
            *report_slot_for_app.borrow_mut() = Some(report);
            drop(entity);
            cx.quit();
        });
    let report = report_slot
        .borrow_mut()
        .take()
        .ok_or(NativeShellError::HeadlessRenderFailed);
    report.map(|mut report| {
        report.profile_root = profile_root;
        report
    })
}

pub struct NativeShell {
    profile: IsolatedDevProfile,
    host_connection: DevTestHostConnection,
    host_runtime: Option<NativeHostRuntimeAttachment>,
    host_state: NativeHostState,
    preferences: RuntimePreferencesSnapshot,
    task_list: TaskList,
    interaction: NativeInteraction,
    keyboard: KeyboardModel,
    last_keyboard_action: Option<KeyboardAction>,
    accessibility_tree: AccessibilityTree,
    terminal: TerminalDockAdapter,
    focus_handle: FocusHandle,
    task_scroll_handle: UniformListScrollHandle,
    controller_task: Option<Task<()>>,
    controller_ticks: usize,
    last_projection_kinds: Vec<NativeHostProjectionKind>,
    pending_preferences: VecDeque<RuntimePreferencesSnapshot>,
    appearance_subscription: Option<Subscription>,
    bounds_subscription: Option<Subscription>,
    platform_accessibility: NativePlatformAccessibilityBridge,
}

impl NativeShell {
    pub fn new(profile: IsolatedDevProfile, cx: &mut Context<Self>) -> Self {
        Self::new_with_host_runtime_and_preferences(
            profile,
            None,
            RuntimePreferencesSnapshot::default(),
            cx,
        )
    }

    /// Construct the real shell tree for deterministic/headless inspection
    /// without leaving a periodic controller task alive after GPUI teardown.
    pub fn new_for_headless(profile: IsolatedDevProfile, cx: &mut Context<Self>) -> Self {
        Self::new_with_attachment_and_state_and_preferences(
            profile,
            None,
            NativeHostState::Disconnected,
            RuntimePreferencesSnapshot::default(),
            cx,
            false,
        )
    }

    pub fn new_with_host_runtime(
        profile: IsolatedDevProfile,
        host_runtime: Option<NativeHostClientRuntime>,
        cx: &mut Context<Self>,
    ) -> Self {
        let host_state = host_runtime
            .as_ref()
            .map(|runtime| NativeHostState::Connected {
                endpoint: runtime.endpoint().to_string(),
            })
            .unwrap_or(NativeHostState::Disconnected);
        Self::new_with_host_runtime_and_state_and_preferences(
            profile,
            host_runtime,
            host_state,
            RuntimePreferencesSnapshot::default(),
            cx,
        )
    }

    pub fn new_with_host_runtime_and_preferences(
        profile: IsolatedDevProfile,
        host_runtime: Option<NativeHostClientRuntime>,
        preferences: RuntimePreferencesSnapshot,
        cx: &mut Context<Self>,
    ) -> Self {
        let host_state = host_runtime
            .as_ref()
            .map(|runtime| NativeHostState::Connected {
                endpoint: runtime.endpoint().to_string(),
            })
            .unwrap_or(NativeHostState::Disconnected);
        Self::new_with_host_runtime_and_state_and_preferences(
            profile,
            host_runtime,
            host_state,
            preferences,
            cx,
        )
    }

    fn new_with_host_runtime_and_state_and_preferences(
        profile: IsolatedDevProfile,
        host_runtime: Option<NativeHostClientRuntime>,
        host_state: NativeHostState,
        preferences: RuntimePreferencesSnapshot,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_attachment_and_state_and_preferences(
            profile,
            host_runtime.map(NativeHostRuntimeAttachment::Client),
            host_state,
            preferences,
            cx,
            true,
        )
    }

    pub fn new_with_host_runtime_port(
        profile: IsolatedDevProfile,
        host_runtime: Box<dyn NativeHostRuntimePort>,
        preferences: RuntimePreferencesSnapshot,
        cx: &mut Context<Self>,
    ) -> Self {
        let host_state = host_runtime.host_state();
        Self::new_with_attachment_and_state_and_preferences(
            profile,
            Some(NativeHostRuntimeAttachment::Injected(host_runtime)),
            host_state,
            preferences,
            cx,
            false,
        )
    }

    fn new_with_attachment_and_state_and_preferences(
        profile: IsolatedDevProfile,
        host_runtime: Option<NativeHostRuntimeAttachment>,
        host_state: NativeHostState,
        preferences: RuntimePreferencesSnapshot,
        cx: &mut Context<Self>,
        start_controller: bool,
    ) -> Self {
        let task_list = TaskList::empty();
        let accessibility_tree = AccessibilityTree::for_task_list(&task_list, None);
        let platform_accessibility = NativePlatformAccessibilityBridge::new(&accessibility_tree);
        let mut shell = Self {
            host_connection: profile.host_connection(),
            profile,
            host_runtime,
            host_state,
            preferences,
            task_list,
            interaction: NativeInteraction::new(None),
            keyboard: KeyboardModel::default(),
            last_keyboard_action: None,
            accessibility_tree,
            terminal: TerminalDockAdapter::unavailable_with_preferences(preferences),
            focus_handle: cx.focus_handle().tab_stop(true),
            task_scroll_handle: UniformListScrollHandle::new(),
            controller_task: None,
            controller_ticks: 0,
            last_projection_kinds: Vec::new(),
            pending_preferences: VecDeque::new(),
            appearance_subscription: None,
            bounds_subscription: None,
            platform_accessibility,
        };
        if start_controller {
            shell.start_controller(cx);
        }
        shell
    }

    pub fn profile(&self) -> &IsolatedDevProfile {
        &self.profile
    }

    pub fn host_connection(&self) -> &DevTestHostConnection {
        &self.host_connection
    }

    pub fn host_endpoint(&self) -> &str {
        self.host_runtime
            .as_ref()
            .map(|attachment| match attachment {
                NativeHostRuntimeAttachment::Client(runtime) => runtime.endpoint(),
                NativeHostRuntimeAttachment::Injected(runtime) => runtime.endpoint(),
            })
            .unwrap_or_else(|| self.host_connection.endpoint())
    }

    pub fn host_state(&self) -> &NativeHostState {
        &self.host_state
    }

    pub fn preferences(&self) -> RuntimePreferencesSnapshot {
        self.preferences
    }

    pub fn last_keyboard_action(&self) -> Option<KeyboardAction> {
        self.last_keyboard_action
    }

    fn start_controller(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(
            |this: gpui::WeakEntity<NativeShell>, cx: &mut gpui::AsyncApp| {
                let mut async_cx = cx.clone();
                async move {
                    loop {
                        Timer::after(CONTROLLER_TICK_INTERVAL).await;
                        if this
                            .update(&mut async_cx, |shell, _cx| {
                                shell.controller_tick(MAX_PENDING_HOST_ACTIONS);
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            },
        );
        self.controller_task = Some(task);
    }

    pub fn controller_tick_for_test(&mut self, max: usize) {
        self.controller_tick(max);
    }

    pub fn controller_tick_count(&self) -> usize {
        self.controller_ticks
    }

    pub fn last_projection_kinds(&self) -> Vec<NativeHostProjectionKind> {
        self.last_projection_kinds.clone()
    }

    pub fn queue_preferences(&mut self, preferences: RuntimePreferencesSnapshot) {
        self.pending_preferences.push_back(preferences);
    }

    pub fn queue_preferences_for_test(&mut self, preferences: RuntimePreferencesSnapshot) {
        self.queue_preferences(preferences);
    }

    fn controller_tick(&mut self, max: usize) {
        self.controller_ticks = self.controller_ticks.saturating_add(1);
        if let Some(preferences) = self.pending_preferences.pop_back() {
            self.pending_preferences.clear();
            self.preferences = preferences;
            self.terminal.set_preferences(preferences);
        }

        let projections = match self.host_runtime.as_mut() {
            Some(NativeHostRuntimeAttachment::Injected(runtime)) => {
                runtime.drain_projection_messages(max)
            }
            Some(NativeHostRuntimeAttachment::Client(runtime)) => {
                runtime.take_ready_projection_messages(max)
            }
            None => Vec::new(),
        };
        if !projections.is_empty() {
            self.last_projection_kinds = projections
                .iter()
                .map(|projection| projection.kind)
                .collect();
        }
        for projection in projections {
            if let Some(task_list) = projection.task_list {
                self.apply_task_list(task_list);
            }
            if let Some(error) = projection.error {
                self.host_state = NativeHostState::Error { message: error };
            }
        }

        let pending = match self.host_runtime.as_mut() {
            Some(NativeHostRuntimeAttachment::Injected(runtime)) => runtime.take_pending(max),
            Some(NativeHostRuntimeAttachment::Client(runtime)) => runtime.take_pending_bounded(max),
            None => Vec::new(),
        };
        for action in pending {
            if !self.interaction.accepts_action_record(&action) {
                continue;
            }
            if let Some(runtime) = self.host_runtime.as_mut() {
                let result = match runtime {
                    NativeHostRuntimeAttachment::Injected(runtime) => {
                        runtime.dispatch_pending(action)
                    }
                    NativeHostRuntimeAttachment::Client(runtime) => {
                        runtime.dispatch_pending(action)
                    }
                };
                if matches!(result, NativeHostActionResult::Disconnected) {
                    self.host_state = NativeHostState::Disconnected;
                }
            }
        }

        let offset = self.task_scroll_handle.0.borrow().base_handle.offset().y / px(1.0);
        let metrics = self.preferences.tokens().density.physical();
        let _ = self.task_list.set_scroll_offset_pixels(
            -offset,
            metrics.row_height as f32 * DEFAULT_VISIBLE_ROWS as f32,
            metrics.row_height as f32,
        );
        self.accessibility_tree =
            AccessibilityTree::for_task_list(&self.task_list, self.interaction.selected_task());
        self.platform_accessibility.sync(&self.accessibility_tree);
    }

    fn install_window_observers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.platform_accessibility.attach_window(window);
        let appearance = cx.observe_window_appearance(window, |shell, window, _cx| {
            shell.queue_preferences(RuntimePreferencesSnapshot::from_system(
                window.appearance(),
                window.scale_factor(),
                shell.preferences.density(),
            ));
        });
        let bounds = cx.observe_window_bounds(window, |shell, window, _cx| {
            shell.queue_preferences(RuntimePreferencesSnapshot::from_system(
                window.appearance(),
                window.scale_factor(),
                shell.preferences.density(),
            ));
        });
        self.appearance_subscription = Some(appearance);
        self.bounds_subscription = Some(bounds);
    }

    pub fn host_runtime(&self) -> Option<&NativeHostClientRuntime> {
        self.host_runtime
            .as_ref()
            .and_then(|attachment| match attachment {
                NativeHostRuntimeAttachment::Client(runtime) => Some(runtime),
                NativeHostRuntimeAttachment::Injected(_) => None,
            })
    }

    pub fn host_runtime_mut(&mut self) -> Option<&mut NativeHostClientRuntime> {
        self.host_runtime
            .as_mut()
            .and_then(|attachment| match attachment {
                NativeHostRuntimeAttachment::Client(runtime) => Some(runtime),
                NativeHostRuntimeAttachment::Injected(_) => None,
            })
    }

    /// Drain only the injected/controller-owned projection queue. The method
    /// is deliberately explicit so GPUI paint/input callbacks cannot perform
    /// transport work; a real [`NativeHostClientRuntime`] uses its async
    /// `drain_bounded` method from the host controller lane.
    pub fn drain_host_projections(&mut self, max: usize) -> Vec<NativeHostProjectionKind> {
        match self.host_runtime.as_mut() {
            Some(NativeHostRuntimeAttachment::Injected(runtime)) => runtime.drain_ready(max),
            Some(NativeHostRuntimeAttachment::Client(runtime)) => {
                runtime.take_ready_projections(max)
            }
            None => Vec::new(),
        }
    }

    /// Controller-lane async drain for the single real client. This is kept
    /// separate from synchronous paint/input APIs so a GPUI callback cannot
    /// accidentally await transport work.
    pub async fn drain_host_projections_async(
        &mut self,
        max: usize,
    ) -> Result<Vec<NativeHostProjectionKind>, IpcError> {
        match self.host_runtime.as_mut() {
            Some(NativeHostRuntimeAttachment::Client(runtime)) => runtime.drain_bounded(max).await,
            Some(NativeHostRuntimeAttachment::Injected(runtime)) => Ok(runtime.drain_ready(max)),
            None => Ok(Vec::new()),
        }
    }

    /// Attach exactly one pre-connected host runtime. The shell never opens
    /// another connection when an attachment is present.
    pub fn attach_host_runtime(
        &mut self,
        host_runtime: NativeHostClientRuntime,
    ) -> Result<(), NativeHostClientRuntime> {
        if self.host_runtime.is_some() {
            return Err(host_runtime);
        }
        self.host_runtime = Some(NativeHostRuntimeAttachment::Client(host_runtime));
        self.host_state = NativeHostState::Connected {
            endpoint: self
                .host_runtime
                .as_ref()
                .and_then(|attachment| match attachment {
                    NativeHostRuntimeAttachment::Client(runtime) => Some(runtime.endpoint()),
                    NativeHostRuntimeAttachment::Injected(_) => None,
                })
                .expect("runtime attached")
                .to_string(),
        };
        Ok(())
    }

    pub fn attach_host_runtime_port(
        &mut self,
        host_runtime: Box<dyn NativeHostRuntimePort>,
    ) -> Result<(), Box<dyn NativeHostRuntimePort>> {
        if self.host_runtime.is_some() {
            return Err(host_runtime);
        }
        self.host_state = host_runtime.host_state();
        self.host_runtime = Some(NativeHostRuntimeAttachment::Injected(host_runtime));
        Ok(())
    }

    pub fn accessibility_tree(&self) -> &AccessibilityTree {
        &self.accessibility_tree
    }

    /// Return the same AccessKit update sent to the Windows adapter. This is
    /// an inspectable platform-tree seam for acceptance tests; it is not a
    /// second accessibility model.
    pub fn platform_accessibility_tree_for_test(&self) -> accesskit::TreeUpdate {
        self.platform_accessibility.tree_update()
    }

    /// Replace the bounded host projection supplied by the inbox attachment.
    /// This is a pure handoff; no client, subscription, or second connection
    /// is created by the shell.
    pub fn apply_task_list(&mut self, task_list: TaskList) {
        let selected_task = self
            .interaction
            .selected_task()
            .filter(|task_id| task_list.task_ids().contains(task_id));
        self.interaction.sync_selected_task(selected_task);
        self.task_list = task_list;
        self.accessibility_tree =
            AccessibilityTree::for_task_list(&self.task_list, self.interaction.selected_task());
        self.platform_accessibility.sync(&self.accessibility_tree);
    }

    pub fn task_list(&self) -> &TaskList {
        &self.task_list
    }

    pub fn rendered_task_count(&self) -> usize {
        self.task_list
            .rendered_task_ids()
            .len()
            .min(MAX_RENDERED_TASK_ROWS)
    }

    fn element_without_handlers(&self) -> impl IntoElement {
        self.element_body(Vec::new())
    }

    fn element_body(&self, task_rows: Vec<AnyElement>) -> impl IntoElement {
        let tokens = self.preferences.tokens();
        let metrics = tokens.density.physical();
        let toolbar = div()
            .id("native-shell-toolbar")
            .w_full()
            .flex()
            .items_center()
            .gap(px(tokens.density.spacing.md))
            .p(px(metrics.control_padding as f32))
            .bg(tokens.surfaces.raised.to_gpui())
            .child(
                div()
                    .id("native-shell-header-title")
                    .text_size(px(18.0))
                    .child("Task Cockpit"),
            )
            .child(
                div()
                    .id("native-shell-header-placeholder")
                    .text_color(tokens.text.secondary.to_gpui())
                    .child("Header"),
            )
            .child(
                Button::new("native-shell-host-status")
                    .label("Host status")
                    .info(),
            )
            .child(
                div()
                    .text_color(tokens.text.secondary.to_gpui())
                    .whitespace_normal()
                    .child(self.host_status_text()),
            );
        let inbox = div()
            .id("native-shell-task-inbox")
            .w_full()
            .flex_col()
            .gap(px(tokens.density.spacing.xs))
            .overflow_y_scroll()
            .child(
                div()
                    .id("native-task-inbox-label")
                    .text_color(tokens.text.secondary.to_gpui())
                    .child("Task Inbox"),
            )
            .children(task_rows);
        let terminal = div()
            .id("native-shell-terminal-dock")
            .w_full()
            .flex_grow()
            .bg(tokens.surfaces.sunken.to_gpui())
            .child(self.terminal.element());
        div()
            .id("native-shell-root")
            .size_full()
            .track_focus(&self.focus_handle)
            .flex_col()
            .bg(tokens.surfaces.canvas.to_gpui())
            .text_color(tokens.text.primary.to_gpui())
            .child(toolbar)
            .child(inbox)
            .child(terminal)
    }

    fn element_with_handlers(&mut self, cx: &Context<Self>) -> impl IntoElement {
        let tokens = self.preferences.tokens();
        let metrics = tokens.density.physical();
        let task_ids = self.task_list.shared_task_ids();
        let shell_entity = cx.entity().downgrade();
        let task_list_element = uniform_list(
            "native-task-uniform-list",
            task_ids.len(),
            move |range, _window, _app| {
                range
                    .filter_map(|index| {
                        task_ids.get(index).copied().map(|task_id| (index, task_id))
                    })
                    .map(|(_index, task_id)| {
                        let shell_for_mouse = shell_entity.clone();
                        let shell_for_key = shell_entity.clone();
                        let mouse_handler =
                            move |event: &MouseDownEvent,
                                  window: &mut Window,
                                  app: &mut gpui::App| {
                                if event.button == MouseButton::Left {
                                    let _ = shell_for_mouse.update(app, |shell, cx| {
                                        cx.stop_propagation();
                                        shell.focus_handle.focus(window);
                                        let _ = shell
                                            .interaction
                                            .navigation_mouse_down(task_id, &shell.task_list);
                                        shell.accessibility_tree = AccessibilityTree::for_task_list(
                                            &shell.task_list,
                                            shell.interaction.selected_task(),
                                        );
                                        shell
                                            .platform_accessibility
                                            .sync(&shell.accessibility_tree);
                                    });
                                }
                            };
                        let key_handler =
                            move |event: &KeyDownEvent,
                                  _window: &mut Window,
                                  app: &mut gpui::App| {
                                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                    let _ = shell_for_key.update(app, |shell, cx| {
                                        cx.stop_propagation();
                                        let _ = shell
                                            .interaction
                                            .navigation_mouse_down(task_id, &shell.task_list);
                                        shell.accessibility_tree = AccessibilityTree::for_task_list(
                                            &shell.task_list,
                                            shell.interaction.selected_task(),
                                        );
                                        shell
                                            .platform_accessibility
                                            .sync(&shell.accessibility_tree);
                                    });
                                }
                            };
                        div()
                            .id(stable_task_element_id(task_id))
                            .tab_stop(true)
                            .w_full()
                            .h(px(metrics.row_height as f32))
                            .p(px(metrics.row_padding as f32))
                            .border_b_1()
                            .border_color(tokens.borders.subtle.to_gpui())
                            .whitespace_normal()
                            .on_mouse_down(MouseButton::Left, mouse_handler)
                            .on_key_down(key_handler)
                            .child(format!("Task {task_id}"))
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(self.task_scroll_handle.clone());

        let terminal_down = cx.listener(|shell, event: &MouseDownEvent, window, cx| {
            cx.stop_propagation();
            shell.focus_handle.focus(window);
            let selected = shell.interaction.selected_task();
            if let Some(task_id) = selected {
                shell.interaction.terminal_mouse_down(
                    NATIVE_POINTER_ID,
                    task_id,
                    PointerButton::Primary,
                    Some(task_id),
                );
            }
            let _ = event;
        });
        let terminal_up = cx.listener(|shell, _event: &MouseUpEvent, _window, cx| {
            cx.stop_propagation();
            shell.interaction.terminal_mouse_up();
        });
        let scroll_handle = self.task_scroll_handle.clone();
        let inbox_scroll = cx.listener(move |shell, event: &ScrollWheelEvent, _window, cx| {
            cx.stop_propagation();
            let delta = event.delta.pixel_delta(px(metrics.row_height as f32)).y;
            let scroll_state = scroll_handle.0.borrow_mut();
            let offset = scroll_state.base_handle.offset();
            scroll_state
                .base_handle
                .set_offset(point(offset.x, offset.y - delta));
            shell.accessibility_tree = AccessibilityTree::for_task_list(
                &shell.task_list,
                shell.interaction.selected_task(),
            );
            shell.platform_accessibility.sync(&shell.accessibility_tree);
        });

        let host_actions = cx.listener(|shell, _action: &HostActions, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_action(ActionRequest::HostActions);
        });
        let host_status = cx.listener(|shell, _action: &HostStatus, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_action(ActionRequest::HostStatus);
        });
        let task_list = cx.listener(|shell, _action: &TaskListAction, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_action(ActionRequest::TaskList);
        });
        let task_show = cx.listener(|shell, _action: &TaskShow, _window, cx| {
            cx.stop_propagation();
            if let Some(task_id) = shell.interaction.selected_task() {
                shell.dispatch_action(ActionRequest::TaskShow { task_id });
            }
        });
        let task_create = cx.listener(|shell, _action: &TaskCreate, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_action(ActionRequest::TaskCreate(
                crate::client::action::TaskCreateArguments {
                    task_id: TaskId::new(),
                    environment_id: crate::domain::id::EnvironmentId::new(),
                    title: "Native shell task".to_string(),
                    description: None,
                    project_id: crate::domain::id::ProjectId::new(),
                    workspace: crate::domain::task::WorkspaceRef::Main,
                },
            ));
        });
        let task_rename = cx.listener(|shell, _action: &TaskRename, _window, cx| {
            cx.stop_propagation();
            if let Some(task_id) = shell.interaction.selected_task() {
                shell.dispatch_action(ActionRequest::TaskRename(
                    crate::client::action::TaskRenameArguments {
                        task_id,
                        title: "Renamed task".to_string(),
                    },
                ));
            }
        });

        let open_palette = cx.listener(|shell, _action: &NativeOpenPalette, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_keyboard(KeyboardShortcut::ctrl(
                crate::ui::actions::ShortcutKey::Character('k'),
            ));
        });
        let open_switcher = cx.listener(|shell, _action: &NativeOpenTaskSwitcher, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_keyboard(KeyboardShortcut::ctrl(
                crate::ui::actions::ShortcutKey::Character('p'),
            ));
        });
        let open_command_palette =
            cx.listener(|shell, _action: &NativeOpenCommandPalette, _window, cx| {
                cx.stop_propagation();
                shell.dispatch_keyboard(KeyboardShortcut::ctrl_shift(
                    crate::ui::actions::ShortcutKey::Character('p'),
                ));
            });
        let open_terminal = cx.listener(|shell, _action: &NativeOpenTerminal, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_keyboard(KeyboardShortcut::ctrl(
                crate::ui::actions::ShortcutKey::Backtick,
            ));
        });
        let dismiss = cx.listener(|shell, _action: &NativeDismissTransient, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_keyboard(KeyboardShortcut::escape());
        });
        let dock_changes = cx.listener(|shell, _action: &NativeDockChanges, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_keyboard(KeyboardShortcut::alt(
                crate::ui::actions::ShortcutKey::Digit(1),
            ));
        });
        let dock_files = cx.listener(|shell, _action: &NativeDockFiles, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_keyboard(KeyboardShortcut::alt(
                crate::ui::actions::ShortcutKey::Digit(2),
            ));
        });
        let dock_terminal = cx.listener(|shell, _action: &NativeDockTerminal, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_keyboard(KeyboardShortcut::alt(
                crate::ui::actions::ShortcutKey::Digit(3),
            ));
        });
        let dock_browser = cx.listener(|shell, _action: &NativeDockBrowser, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_keyboard(KeyboardShortcut::alt(
                crate::ui::actions::ShortcutKey::Digit(4),
            ));
        });
        let dock_services = cx.listener(|shell, _action: &NativeDockServices, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_keyboard(KeyboardShortcut::alt(
                crate::ui::actions::ShortcutKey::Digit(5),
            ));
        });
        let dock_artifacts = cx.listener(|shell, _action: &NativeDockArtifacts, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_keyboard(KeyboardShortcut::alt(
                crate::ui::actions::ShortcutKey::Digit(6),
            ));
        });
        let dock_review = cx.listener(|shell, _action: &NativeDockReview, _window, cx| {
            cx.stop_propagation();
            shell.dispatch_keyboard(KeyboardShortcut::alt(
                crate::ui::actions::ShortcutKey::Digit(7),
            ));
        });

        div()
            .id("native-shell-root")
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action::<HostActions>(host_actions)
            .on_action::<HostStatus>(host_status)
            .on_action::<TaskListAction>(task_list)
            .on_action::<TaskShow>(task_show)
            .on_action::<TaskCreate>(task_create)
            .on_action::<TaskRename>(task_rename)
            .on_action::<NativeOpenPalette>(open_palette)
            .on_action::<NativeOpenTaskSwitcher>(open_switcher)
            .on_action::<NativeOpenCommandPalette>(open_command_palette)
            .on_action::<NativeOpenTerminal>(open_terminal)
            .on_action::<NativeDismissTransient>(dismiss)
            .on_action::<NativeDockChanges>(dock_changes)
            .on_action::<NativeDockFiles>(dock_files)
            .on_action::<NativeDockTerminal>(dock_terminal)
            .on_action::<NativeDockBrowser>(dock_browser)
            .on_action::<NativeDockServices>(dock_services)
            .on_action::<NativeDockArtifacts>(dock_artifacts)
            .on_action::<NativeDockReview>(dock_review)
            .flex_col()
            .bg(tokens.surfaces.canvas.to_gpui())
            .text_color(tokens.text.primary.to_gpui())
            .child(
                div()
                    .id("native-shell-toolbar")
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(tokens.density.spacing.md))
                    .p(px(metrics.control_padding as f32))
                    .bg(tokens.surfaces.raised.to_gpui())
                    .child(
                        div()
                            .id("native-shell-header-title")
                            .text_size(px(18.0))
                            .child("Task Cockpit"),
                    )
                    .child(
                        div()
                            .id("native-shell-header-placeholder")
                            .text_color(tokens.text.secondary.to_gpui())
                            .child("Header"),
                    )
                    .child(
                        Button::new("native-shell-host-status")
                            .label("Host status")
                            .info()
                            .on_click(cx.listener(|shell, _event: &ClickEvent, _window, cx| {
                                cx.stop_propagation();
                                shell.dispatch_pointer_action(
                                    ActionRequest::HostStatus,
                                    NATIVE_POINTER_ID,
                                );
                            })),
                    )
                    .child(self.host_status_text()),
            )
            .child(
                div()
                    .id("native-shell-task-inbox")
                    .w_full()
                    .flex_col()
                    .gap(px(tokens.density.spacing.xs))
                    .on_scroll_wheel(inbox_scroll)
                    .child(
                        div()
                            .id("native-task-inbox-label")
                            .text_color(tokens.text.secondary.to_gpui())
                            .child("Task Inbox"),
                    )
                    .child(task_list_element),
            )
            .child(
                div()
                    .id("native-shell-terminal-dock")
                    .w_full()
                    .flex_grow()
                    .capture_any_mouse_down(terminal_down)
                    .capture_any_mouse_up(terminal_up)
                    .bg(tokens.surfaces.sunken.to_gpui())
                    .child(self.terminal.element()),
            )
    }

    pub fn dispatch_action_for_test(&mut self, request: ActionRequest) {
        self.dispatch_action(request);
    }

    fn dispatch_action(&mut self, request: ActionRequest) {
        let Some(record) = self.interaction.action(request) else {
            return;
        };
        let _ = self.enqueue_host_action(record);
    }

    fn dispatch_pointer_action(&mut self, request: ActionRequest, pointer_id: u64) {
        let Some(record) = self
            .interaction
            .action_from_source(request, ActivationSource::Pointer { pointer_id })
        else {
            return;
        };
        let _ = self.enqueue_host_action(record);
    }

    fn dispatch_keyboard(&mut self, shortcut: KeyboardShortcut) {
        let Some((focus_epoch, request_generation, action)) =
            self.interaction.keyboard(&self.keyboard, shortcut)
        else {
            return;
        };
        if self
            .interaction
            .commit_keyboard_action(focus_epoch, request_generation, action)
        {
            self.last_keyboard_action = Some(action);
        }
    }

    fn enqueue_host_action(&mut self, record: NativeActionRecord) -> NativeHostActionResult {
        match self.host_runtime.as_mut() {
            Some(NativeHostRuntimeAttachment::Client(runtime)) => runtime.enqueue(record),
            Some(NativeHostRuntimeAttachment::Injected(runtime)) => runtime.enqueue(record),
            None => NativeHostActionResult::Disconnected,
        }
    }

    fn host_status_text(&self) -> String {
        match &self.host_state {
            NativeHostState::Connected { .. } => "Connected · host".to_string(),
            NativeHostState::Disconnected => "Disconnected · host".to_string(),
            NativeHostState::Error { .. } => "Error · host".to_string(),
        }
    }
}

impl Render for NativeShell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.element_with_handlers(cx)
    }
}

/// Launch the actual native GPUI shell with only the generated isolated
/// dev/test profile. The preview CLI remains a separate host-free path.
pub fn run_native_shell(workspace_root: impl AsRef<Path>) -> Result<(), NativeShellError> {
    let profile = isolated_dev_profile(workspace_root)?;
    let mut bootstrap = ProcessNativeHostBootstrap;
    run_native_shell_with_bootstrap(profile, &mut bootstrap)
}

pub fn run_native_shell_with_bootstrap(
    profile: IsolatedDevProfile,
    bootstrap: &mut dyn NativeHostBootstrap,
) -> Result<(), NativeShellError> {
    let (host_runtime, host_state) = match bootstrap.start(&profile) {
        Ok(runtime) => {
            let endpoint = match &runtime {
                NativeHostRuntimeAttachment::Client(runtime) => runtime.endpoint().to_string(),
                NativeHostRuntimeAttachment::Injected(runtime) => runtime.endpoint().to_string(),
            };
            (Some(runtime), NativeHostState::Connected { endpoint })
        }
        Err(NativeShellError::HostConnect { message }) => (
            None,
            NativeHostState::Error {
                message: bounded_host_error(message),
            },
        ),
        Err(error) => (
            None,
            NativeHostState::Error {
                message: bounded_host_error(error.to_string()),
            },
        ),
    };
    launch_native_shell(profile, host_runtime, host_state)
}

/// Launch the native shell with one caller-owned host runtime attachment.
///
/// The attachment is moved into the shell entity exactly once. Header and
/// inbox owners can share its controller/task lane through their projection
/// seams; this function never creates a second `HostClient`.
pub fn run_native_shell_with_runtime(
    workspace_root: impl AsRef<Path>,
    host_runtime: Option<NativeHostClientRuntime>,
) -> Result<(), NativeShellError> {
    let profile = isolated_dev_profile(workspace_root)?;
    let host_state = host_runtime
        .as_ref()
        .map(|runtime| NativeHostState::Connected {
            endpoint: runtime.endpoint().to_string(),
        })
        .unwrap_or(NativeHostState::Disconnected);
    launch_native_shell(
        profile,
        host_runtime.map(NativeHostRuntimeAttachment::Client),
        host_state,
    )
}

fn launch_native_shell(
    profile: IsolatedDevProfile,
    host_runtime: Option<NativeHostRuntimeAttachment>,
    host_state: NativeHostState,
) -> Result<(), NativeShellError> {
    eprintln!(
        "devmanager-next native shell profile: {}",
        profile.root().display()
    );
    let error_slot = Rc::new(RefCell::new(None));
    let error_slot_for_app = Rc::clone(&error_slot);
    Application::new()
        .with_assets(AppAssets::new())
        .run(move |cx| {
            crate::ui::init(cx);
            actions::register_native_keyboard_bindings(cx);
            let profile_for_window = profile.clone();
            let host_runtime_for_window = host_runtime;
            let host_state_for_window = host_state;
            let result = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::centered(size(px(1_280.0), px(800.0)), cx)),
                    ..WindowOptions::default()
                },
                move |window, cx| {
                    let preferences = RuntimePreferencesSnapshot::from_system(
                        window.appearance(),
                        window.scale_factor(),
                        RuntimePreferencesSnapshot::default().density(),
                    );
                    let entity = cx.new(|cx| {
                        NativeShell::new_with_attachment_and_state_and_preferences(
                            profile_for_window,
                            host_runtime_for_window,
                            host_state_for_window,
                            preferences,
                            cx,
                            true,
                        )
                    });
                    let _ = entity.update(cx, |shell, cx| {
                        shell.install_window_observers(window, cx);
                    });
                    entity
                },
            );
            if let Err(error) = result {
                *error_slot_for_app.borrow_mut() = Some(NativeShellError::WindowOpen {
                    message: error.to_string(),
                });
                cx.quit();
            } else {
                cx.activate(true);
            }
        });
    let error = error_slot.borrow_mut().take().map_or(Ok(()), Err);
    error
}

fn bounded_host_error(message: String) -> String {
    const MAX_HOST_ERROR_CHARS: usize = 256;
    message.chars().take(MAX_HOST_ERROR_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::{ensure_isolated_host_config_base, isolated_dev_profile};

    #[test]
    fn default_host_bootstrap_prepares_only_the_isolated_config_base() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");

        assert!(!profile.host_config_base().exists());
        ensure_isolated_host_config_base(&profile).expect("isolated config base");
        assert!(profile.host_config_base().is_dir());
        assert!(std::fs::canonicalize(profile.host_config_base())
            .expect("canonical isolated config base")
            .starts_with(profile.workspace_root()));
    }
}

#[allow(dead_code)]
fn _terminal_adapter_dependency() -> &'static str {
    TERMINAL_ADAPTER_DEPENDENCY
}
