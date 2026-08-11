//! The real native Task Cockpit shell entrypoint and its isolated host seam.
//!
//! This module deliberately owns only a local projection. It does not open the
//! installed profile, read the production session, start a legacy app, or
//! embed a WebView. A later inbox/host owner can provide a `ClientModel` and a
//! complete terminal model through the explicit seams below.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::env;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gpui::{
    div, point, px, size, uniform_list, AnyElement, AppContext, Application, ClickEvent, Context,
    ElementId, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseUpEvent, ParentElement, Render, ScrollWheelEvent,
    StatefulInteractiveElement, Styled, Subscription, Task, Timer, UniformListScrollHandle, Window,
    WindowBounds, WindowOptions,
};
use gpui_component::button::{Button, ButtonVariants};
use uuid::Uuid;

use crate::assets::AppAssets;
use crate::client::action;
use crate::client::{
    ClientModel, ClientSubscription, HostClient, HostClientConfig, SubscriptionUpdate,
};
use crate::domain::id::{CommandId, TaskId};
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
use crate::ui::task_cockpit::{Inbox, TaskList, DEFAULT_VISIBLE_ROWS, FIXED_VIRTUAL_OVERSCAN};
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
const MAX_ACCESSIBILITY_ACTIONS: usize = 32;
const MAX_PENDING_PREFERENCES: usize = 8;
const CONTROLLER_TICK_INTERVAL: Duration = Duration::from_millis(16);
const NATIVE_SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);
const NATIVE_STARTUP_BUDGET: Duration = Duration::from_secs(5);
const MAX_RETAINED_WORKERS: usize = 8;
const MAX_RETAINED_CHILDREN: usize = 8;

/// One absolute budget shared by worker, subscription, child, and runtime
/// cleanup.  A fresh timeout for each phase would allow a hung phase to turn
/// shutdown into an unbounded chain of waits.
#[derive(Clone, Copy, Debug)]
struct NativeShutdownDeadline {
    at: Instant,
}

impl NativeShutdownDeadline {
    fn from_now(budget: Duration) -> Self {
        Self {
            at: Instant::now() + budget,
        }
    }

    fn until(at: Instant) -> Self {
        Self { at }
    }

    fn remaining(self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    fn expired(self) -> bool {
        self.remaining().is_zero()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReaperKind {
    Worker,
    Child,
}

#[derive(Debug, Default)]
struct ReaperCounts {
    workers: usize,
    children: usize,
}

#[derive(Debug)]
struct ReaperPermit {
    kind: ReaperKind,
}

fn reaper_counts() -> &'static Mutex<ReaperCounts> {
    static COUNTS: OnceLock<Mutex<ReaperCounts>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(ReaperCounts::default()))
}

fn acquire_reaper_permit(kind: ReaperKind) -> Option<ReaperPermit> {
    // A timed-out owner remains in the bounded reaper until its handle exits.
    // Reap before checking the cap so a later launch can recover immediately
    // instead of permanently treating completed work as live capacity.
    match kind {
        ReaperKind::Worker => reap_retained_workers(),
        ReaperKind::Child => reap_retained_children(),
    }
    let mut counts = reaper_counts()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (count, limit) = match kind {
        ReaperKind::Worker => (&mut counts.workers, MAX_RETAINED_WORKERS),
        ReaperKind::Child => (&mut counts.children, MAX_RETAINED_CHILDREN),
    };
    if *count >= limit {
        return None;
    }
    *count += 1;
    Some(ReaperPermit { kind })
}

impl Drop for ReaperPermit {
    fn drop(&mut self) {
        let mut counts = reaper_counts()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match self.kind {
            ReaperKind::Worker => counts.workers = counts.workers.saturating_sub(1),
            ReaperKind::Child => counts.children = counts.children.saturating_sub(1),
        }
    }
}

struct OwnedWorker {
    handle: JoinHandle<()>,
    _permit: ReaperPermit,
}

struct OwnedChild {
    child: Child,
    _permit: ReaperPermit,
}

fn retained_workers() -> &'static Mutex<Vec<OwnedWorker>> {
    static RETAINED: OnceLock<Mutex<Vec<OwnedWorker>>> = OnceLock::new();
    RETAINED.get_or_init(|| Mutex::new(Vec::new()))
}

fn retained_children() -> &'static Mutex<Vec<OwnedChild>> {
    static RETAINED: OnceLock<Mutex<Vec<OwnedChild>>> = OnceLock::new();
    RETAINED.get_or_init(|| Mutex::new(Vec::new()))
}

fn reap_retained_workers() {
    let mut workers = retained_workers()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut pending = Vec::with_capacity(workers.len());
    for worker in workers.drain(..) {
        if worker.handle.is_finished() {
            // Joining an already finished worker cannot block and proves
            // ownership was reaped rather than detached.
            let _ = worker.handle.join();
        } else {
            pending.push(worker);
        }
    }
    *workers = pending;
}

fn reap_retained_children() {
    let mut children = retained_children()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    children.retain_mut(|child| match child.child.try_wait() {
        Ok(Some(_)) | Err(_) => false,
        Ok(None) => true,
    });
}

fn retain_worker(worker: OwnedWorker) {
    let mut workers = retained_workers()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    debug_assert!(workers.len() < MAX_RETAINED_WORKERS);
    workers.push(worker);
}

fn retain_child(child: OwnedChild) {
    let mut children = retained_children()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    debug_assert!(children.len() < MAX_RETAINED_CHILDREN);
    children.push(child);
}

fn join_worker_with_deadline(worker: OwnedWorker, deadline: NativeShutdownDeadline) {
    reap_retained_workers();
    let mut worker = Some(worker);
    while !deadline.expired() {
        if worker
            .as_ref()
            .is_some_and(|worker| worker.handle.is_finished())
        {
            let _ = worker.take().expect("finished worker handle").handle.join();
            return;
        }
        std::thread::sleep(Duration::from_millis(2).min(deadline.remaining()));
    }
    if let Some(worker) = worker {
        retain_worker(worker);
    }
}

fn terminate_child_with_deadline(mut child: OwnedChild, deadline: NativeShutdownDeadline) {
    reap_retained_children();
    let _ = child.child.kill();
    while !deadline.expired() {
        match child.child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(2).min(deadline.remaining())),
        }
    }
    retain_child(child);
}

fn stable_task_element_id(task_id: TaskId) -> ElementId {
    // GPUI has a UUID element identity, so retain all 128 bits of the domain
    // TaskId instead of collapsing it to an offset or a lossy hash.
    ElementId::Uuid(Uuid::from_bytes(*task_id.as_bytes()))
}

fn pointer_button(button: MouseButton) -> Option<PointerButton> {
    match button {
        MouseButton::Left => Some(PointerButton::Primary),
        MouseButton::Right => Some(PointerButton::Secondary),
        MouseButton::Middle => Some(PointerButton::Middle),
        MouseButton::Navigate(_) => None,
    }
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
    Child(OwnedChild),
    Empty,
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
                    NativeHostProcessKind::Empty => "empty",
                },
            )
            .finish()
    }
}

impl Drop for NativeHostProcess {
    fn drop(&mut self) {
        self.terminate(NativeShutdownDeadline::from_now(NATIVE_SHUTDOWN_BUDGET));
    }
}

impl NativeHostProcess {
    fn terminate(&mut self, deadline: NativeShutdownDeadline) {
        let kind = std::mem::replace(&mut self.kind, NativeHostProcessKind::Empty);
        if let NativeHostProcessKind::Child(child) = kind {
            terminate_child_with_deadline(child, deadline);
        }
    }
}

pub(crate) trait NativeHostBootstrap {
    fn start_until(
        &mut self,
        profile: &IsolatedDevProfile,
        deadline: Instant,
    ) -> Result<NativeHostRuntimeAttachment, NativeShellError>;
}

#[derive(Debug, Default)]
pub(crate) struct ProcessNativeHostBootstrap;

impl NativeHostBootstrap for ProcessNativeHostBootstrap {
    fn start_until(
        &mut self,
        profile: &IsolatedDevProfile,
        deadline: Instant,
    ) -> Result<NativeHostRuntimeAttachment, NativeShellError> {
        let deadline = NativeShutdownDeadline::until(deadline);
        if deadline.expired() {
            return Err(NativeShellError::HostConnect {
                message: "native host startup deadline expired before launch".to_string(),
            });
        }
        ensure_isolated_host_config_base(profile)?;
        if deadline.expired() {
            return Err(NativeShellError::HostConnect {
                message: "native host startup deadline expired before launch".to_string(),
            });
        }
        let spec = NativeHostLaunchSpec::for_profile(profile, std::process::id())?;
        let permit = acquire_reaper_permit(ReaperKind::Child).ok_or_else(|| {
            NativeShellError::HostConnect {
                message: "native host child reaper capacity exhausted".to_string(),
            }
        })?;
        if deadline.expired() {
            return Err(NativeShellError::HostConnect {
                message: "native host startup deadline expired before launch".to_string(),
            });
        }
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
            kind: NativeHostProcessKind::Child(OwnedChild {
                child,
                _permit: permit,
            }),
        };
        let runtime =
            NativeHostClientRuntime::connect_blocking_with_process(profile, process, deadline)?;
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
    pub action_epoch: u64,
    pub consumed: bool,
    pub propagation_stopped: bool,
}

/// All identities that make a native action valid.  The shell captures these
/// values at the event boundary and compares every value again on dispatch;
/// matching only a focus or request counter permits stale actions to cross a
/// reconnect, task resync, or runtime replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeActionEpochs {
    pub connection_epoch: u64,
    pub client_epoch: u64,
    pub navigation_epoch: u64,
    pub resource_generation: u64,
    pub runtime_generation: u64,
    pub action_epoch: u64,
    pub focus_epoch: FocusEpoch,
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
    /// Monotonic UI action capture. A later action invalidates an older one.
    pub action_epoch: u64,
    pub connection_epoch: u64,
    pub client_epoch: u64,
    pub navigation_epoch: u64,
    pub resource_generation: u64,
    pub runtime_generation: u64,
    /// Task identity captured at activation time, never reconstructed by the
    /// transport worker from mutable selection state.
    pub task_id: Option<TaskId>,
    /// Durable task revision and task action epoch observed by the immutable
    /// client model when this action was captured.
    pub expected_task_revision: Option<u64>,
    pub captured_task_action_epoch: Option<u64>,
    /// Capability required by the catalog entry and an explicit disabled
    /// reason when a caller ever records a disabled action.
    pub capability: Option<Capability>,
    pub disabled_reason: Option<String>,
    pub event: ActionEvent,
    /// Every captured action has an explicit typed host command or a typed
    /// HOLD. Read-only actions must never disappear as `None` at dispatch.
    pub command: NativeHostCommand,
}

#[derive(Clone, Debug)]
pub enum NativeHostCommand {
    Envelope(crate::domain::command::CommandEnvelope),
    TaskCreate {
        arguments: crate::client::action::TaskCreateArguments,
        command_id: CommandId,
        issued_at_ms: i64,
    },
    TaskRename {
        arguments: crate::client::action::TaskRenameArguments,
        expected_task_revision: u64,
        command_id: CommandId,
        issued_at_ms: i64,
    },
    /// Explicitly surfaced until the canonical host query/request adapter is
    /// available. This is intentionally typed so the action is not silently
    /// ignored by the worker.
    Hold {
        action_id: &'static str,
        reason: &'static str,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativeHostRuntimeEpochs {
    pub connection_epoch: u64,
    pub resource_generation: u64,
    pub runtime_generation: u64,
}

impl NativeHostRuntimeEpochs {
    fn initial() -> Self {
        Self {
            connection_epoch: 1,
            resource_generation: 1,
            runtime_generation: 1,
        }
    }
}

fn current_runtime_epochs(epochs: &Arc<Mutex<NativeHostRuntimeEpochs>>) -> NativeHostRuntimeEpochs {
    epochs
        .lock()
        .map(|epochs| *epochs)
        .unwrap_or_else(|poisoned| *poisoned.into_inner())
}

fn bump_connection_epoch(epochs: &Arc<Mutex<NativeHostRuntimeEpochs>>) -> NativeHostRuntimeEpochs {
    let mut current = epochs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    current.connection_epoch = current.connection_epoch.saturating_add(1);
    current.runtime_generation = current.runtime_generation.saturating_add(1);
    *current
}

fn bump_resource_generation(
    epochs: &Arc<Mutex<NativeHostRuntimeEpochs>>,
) -> NativeHostRuntimeEpochs {
    let mut current = epochs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    current.resource_generation = current.resource_generation.saturating_add(1);
    *current
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeHostProjection {
    pub kind: NativeHostProjectionKind,
    pub client_model: Option<Arc<ClientModel>>,
    pub error: Option<String>,
    pub epochs: Option<NativeHostRuntimeEpochs>,
}

enum NativeHostWorkerCommand {
    Execute(NativeActionRecord),
    Shutdown,
}

/// Borrowed seam for the canonical header projection. The shell owns only the
/// attachment lifecycle; the later header component can convert its bounded
/// immutable projection into this value without opening another client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeHeaderAttachment {
    Unavailable {
        reason: String,
    },
    Projection {
        title: String,
        status: String,
        remote: String,
        quota: String,
    },
}

impl NativeHeaderAttachment {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: bounded_header_text(reason.into()),
        }
    }

    pub fn projection(
        title: impl Into<String>,
        status: impl Into<String>,
        remote: impl Into<String>,
        quota: impl Into<String>,
    ) -> Self {
        Self::Projection {
            title: bounded_header_text(title.into()),
            status: bounded_header_text(status.into()),
            remote: bounded_header_text(remote.into()),
            quota: bounded_header_text(quota.into()),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Unavailable { reason } => format!("Header unavailable: {reason}"),
            Self::Projection { title, .. } => title.clone(),
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::Unavailable { .. } => {
                "The canonical header projection is not attached to this shell.".to_string()
            }
            Self::Projection {
                status,
                remote,
                quota,
                ..
            } => format!("{status} · {remote} · {quota}")
                .chars()
                .take(512)
                .collect(),
        }
    }
}

impl Default for NativeHeaderAttachment {
    fn default() -> Self {
        Self::unavailable("waiting for the canonical client-model attachment")
    }
}

fn bounded_header_text(value: String) -> String {
    const MAX_HEADER_TEXT_SCALARS: usize = 256;
    value.chars().take(MAX_HEADER_TEXT_SCALARS).collect()
}

impl NativeHostProjection {
    pub fn kind(kind: NativeHostProjectionKind) -> Self {
        Self {
            kind,
            client_model: None,
            error: None,
            epochs: None,
        }
    }

    pub fn model(kind: NativeHostProjectionKind, model: Arc<ClientModel>) -> Self {
        Self {
            kind,
            client_model: Some(model),
            error: None,
            epochs: None,
        }
    }

    pub fn client_model(model: Arc<ClientModel>) -> Self {
        Self::model(NativeHostProjectionKind::Snapshot, model)
    }

    fn at_epochs(mut self, epochs: NativeHostRuntimeEpochs) -> Self {
        self.epochs = Some(epochs);
        self
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
    subscription: Arc<Mutex<ClientSubscription>>,
    client_model: Arc<Mutex<Option<Arc<ClientModel>>>>,
    bootstrapped: Arc<AtomicBool>,
    pending: VecDeque<NativeActionRecord>,
    ready_projections: Arc<Mutex<VecDeque<NativeHostProjection>>>,
    command_tx: SyncSender<NativeHostWorkerCommand>,
    cancellation: Arc<AtomicBool>,
    worker: Option<OwnedWorker>,
    epochs: Arc<Mutex<NativeHostRuntimeEpochs>>,
    runtime_guard: Option<Arc<tokio::runtime::Runtime>>,
    host_process: Option<NativeHostProcess>,
}

mod native_host_runtime_sealed {
    pub trait Sealed {}
}

/// Crate-private compatibility port for the preview adapter. It is sealed and
/// implemented only by the real [`NativeHostClientRuntime`]; no external
/// caller can inject a transport, projection queue, epoch source, or action
/// dispatcher into the production shell.
pub(crate) trait NativeHostRuntimePort: native_host_runtime_sealed::Sealed + Send {
    fn endpoint(&self) -> &str;
    fn host_state(&self) -> NativeHostState;
    fn epochs(&self) -> NativeHostRuntimeEpochs;
    fn enqueue(&mut self, action: NativeActionRecord) -> NativeHostActionResult;
    fn drain_ready(&mut self, max: usize) -> Vec<NativeHostProjectionKind>;
    fn drain_projection_messages(&mut self, max: usize) -> Vec<NativeHostProjection>;
    fn take_pending(&mut self, max: usize) -> Vec<NativeActionRecord>;
    fn dispatch_pending(&mut self, action: NativeActionRecord) -> NativeHostActionResult;
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
        let deadline = NativeShutdownDeadline::from_now(NATIVE_STARTUP_BUDGET);
        let client = tokio::time::timeout(
            deadline.remaining(),
            HostClient::connect(profile.host_client_config()),
        )
        .await
        .map_err(|_| NativeShellError::HostConnect {
            message: "native host connection deadline expired".to_string(),
        })?
        .map_err(|error| NativeShellError::HostConnect {
            message: error.to_string(),
        })?;
        let mut runtime = Self::new(client)?;
        tokio::time::timeout(deadline.remaining(), runtime.bootstrap_projection())
            .await
            .map_err(|_| NativeShellError::HostConnect {
                message: "native host bootstrap deadline expired".to_string(),
            })??;
        Ok(runtime)
    }

    /// Synchronous bootstrap for the default binary. The multi-thread Tokio
    /// runtime remains owned by this one client owner so the connection's
    /// reader/writer tasks continue draining while GPUI paints and handles
    /// input. A failed attempt becomes a typed shell error at the call site.
    pub fn connect_blocking(profile: &IsolatedDevProfile) -> Result<Self, NativeShellError> {
        let deadline = NativeShutdownDeadline::from_now(NATIVE_STARTUP_BUDGET);
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|error| NativeShellError::HostConnect {
                    message: format!("runtime bootstrap failed: {error}"),
                })?,
        );
        let client = runtime.block_on(tokio::time::timeout(
            deadline.remaining(),
            HostClient::connect(profile.host_client_config()),
        ));
        let client = client.map_err(|_| NativeShellError::HostConnect {
            message: "native host connection deadline expired".to_string(),
        })?;
        let client = client.map_err(|error| NativeShellError::HostConnect {
            message: error.to_string(),
        })?;
        let mut runtime_owner = match Self::new_with_runtime(client, runtime.clone()) {
            Ok(runtime_owner) => runtime_owner,
            Err(error) => {
                if let Ok(runtime) = Arc::try_unwrap(runtime) {
                    runtime.shutdown_timeout(deadline.remaining());
                }
                return Err(error);
            }
        };
        runtime
            .block_on(tokio::time::timeout(
                deadline.remaining(),
                runtime_owner.bootstrap_projection(),
            ))
            .map_err(|_| NativeShellError::HostConnect {
                message: "native host bootstrap deadline expired".to_string(),
            })?
            .map_err(|error| NativeShellError::HostConnect {
                message: error.to_string(),
            })?;
        Ok(runtime_owner)
    }

    fn connect_blocking_with_process(
        profile: &IsolatedDevProfile,
        process: NativeHostProcess,
        deadline: NativeShutdownDeadline,
    ) -> Result<Self, NativeShellError> {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
        {
            Ok(runtime) => Arc::new(runtime),
            Err(error) => {
                let mut process = process;
                process.terminate(deadline);
                return Err(NativeShellError::HostConnect {
                    message: format!("runtime bootstrap failed: {error}"),
                });
            }
        };
        let client = match runtime.block_on(connect_with_startup_retry(profile, deadline)) {
            Ok(client) => client,
            Err(error) => {
                let mut process = process;
                process.terminate(deadline);
                return Err(NativeShellError::HostConnect {
                    message: error.to_string(),
                });
            }
        };
        let mut runtime_owner =
            match Self::new_with_runtime_and_process(client, runtime.clone(), process) {
                Ok(runtime_owner) => runtime_owner,
                Err((error, mut process)) => {
                    process.terminate(deadline);
                    return Err(error);
                }
            };
        let bootstrap = runtime
            .block_on(tokio::time::timeout(
                deadline.remaining(),
                runtime_owner.bootstrap_projection(),
            ))
            .map_err(|_| NativeShellError::HostConnect {
                message: "native host bootstrap deadline expired".to_string(),
            })
            .and_then(|result| {
                result.map_err(|error| NativeShellError::HostConnect {
                    message: error.to_string(),
                })
            });
        if let Err(error) = bootstrap {
            if let Some(process) = runtime_owner.host_process.as_mut() {
                process.terminate(deadline);
            }
            return Err(error);
        }
        Ok(runtime_owner)
    }

    pub fn new(client: HostClient) -> Result<Self, NativeShellError> {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|error| NativeShellError::HostConnect {
                    message: format!("runtime bootstrap failed: {error}"),
                })?,
        );
        Self::new_with_runtime(client, runtime)
    }

    fn new_with_runtime(
        client: HostClient,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Result<Self, NativeShellError> {
        Self::new_with_runtime_guard(client, Some(runtime))
    }

    fn new_with_runtime_guard(
        client: HostClient,
        runtime_guard: Option<Arc<tokio::runtime::Runtime>>,
    ) -> Result<Self, NativeShellError> {
        let endpoint = client.endpoint().to_string();
        let client = Arc::new(Mutex::new(client));
        let subscription = Arc::new(Mutex::new(ClientSubscription::new()));
        let client_model = Arc::new(Mutex::new(None));
        let bootstrapped = Arc::new(AtomicBool::new(false));
        let cancellation = Arc::new(AtomicBool::new(false));
        let (command_tx, command_rx) = std::sync::mpsc::sync_channel(MAX_PENDING_HOST_ACTIONS);
        let epochs = Arc::new(Mutex::new(NativeHostRuntimeEpochs::initial()));
        let client_for_worker = Arc::clone(&client);
        let subscription_for_worker = Arc::clone(&subscription);
        let client_model_for_worker = Arc::clone(&client_model);
        let bootstrapped_for_worker = Arc::clone(&bootstrapped);
        let runtime_for_worker = runtime_guard.clone();
        let cancellation_for_worker = Arc::clone(&cancellation);
        let epochs_for_worker = Arc::clone(&epochs);
        let projections_for_worker = Arc::new(Mutex::new(VecDeque::new()));
        let projections_for_worker_thread = Arc::clone(&projections_for_worker);
        let worker = if runtime_guard.is_some() {
            let permit = acquire_reaper_permit(ReaperKind::Worker).ok_or_else(|| {
                NativeShellError::HostConnect {
                    message: "native host worker reaper capacity exhausted".to_string(),
                }
            })?;
            let handle = std::thread::Builder::new()
                .name("devmanager-native-host-worker".to_string())
                .spawn(move || {
                    native_host_worker_loop(
                        client_for_worker,
                        subscription_for_worker,
                        client_model_for_worker,
                        bootstrapped_for_worker,
                        runtime_for_worker,
                        cancellation_for_worker,
                        epochs_for_worker,
                        command_rx,
                        projections_for_worker_thread,
                    )
                })
                .map_err(|error| NativeShellError::HostConnect {
                    message: format!("native host worker spawn failed: {error}"),
                })?;
            Some(OwnedWorker {
                handle,
                _permit: permit,
            })
        } else {
            None
        };
        Ok(Self {
            endpoint,
            client,
            subscription,
            client_model,
            bootstrapped,
            pending: VecDeque::new(),
            ready_projections: projections_for_worker,
            command_tx,
            cancellation,
            worker,
            epochs,
            runtime_guard,
            host_process: None,
        })
    }

    fn new_with_runtime_and_process(
        client: HostClient,
        runtime: Arc<tokio::runtime::Runtime>,
        process: NativeHostProcess,
    ) -> Result<Self, (NativeShellError, NativeHostProcess)> {
        let mut runtime_owner = match Self::new_with_runtime_guard(client, Some(runtime)) {
            Ok(runtime_owner) => runtime_owner,
            Err(error) => return Err((error, process)),
        };
        runtime_owner.host_process = Some(process);
        Ok(runtime_owner)
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

    /// Perform the initial bounded five-section snapshot/replay handoff owned
    /// by the one client subscription. GPUI receives an immutable model; it
    /// never observes raw snapshot pages or a task-only transport projection.
    pub async fn bootstrap_projection(
        &mut self,
    ) -> Result<Vec<NativeHostProjectionKind>, NativeShellError> {
        {
            let mut client = self
                .client
                .lock()
                .map_err(|_| NativeShellError::HostConnect {
                    message: "native host client lock poisoned".to_string(),
                })?;
            let mut subscription =
                self.subscription
                    .lock()
                    .map_err(|_| NativeShellError::HostConnect {
                        message: "native host subscription lock poisoned".to_string(),
                    })?;
            subscription
                .synchronize(&mut client)
                .await
                .map_err(|error| NativeShellError::HostConnect {
                    message: error.to_string(),
                })?;
            let model = Arc::new(subscription.model().cloned().ok_or_else(|| {
                NativeShellError::HostConnect {
                    message: "native host subscription produced no client model".to_string(),
                }
            })?);
            if let Ok(mut current) = self.client_model.lock() {
                *current = Some(Arc::clone(&model));
            }
            let epochs = current_runtime_epochs(&self.epochs);
            if let Ok(mut projections) = self.ready_projections.lock() {
                if projections.len() < MAX_HOST_PROJECTIONS {
                    projections.push_back(
                        NativeHostProjection::client_model(Arc::clone(&model)).at_epochs(epochs),
                    );
                }
                let remaining = MAX_HOST_PROJECTIONS.saturating_sub(projections.len());
                projections.extend(
                    std::iter::once(NativeHostProjectionKind::Replay)
                        .take(remaining)
                        .map(NativeHostProjection::kind)
                        .map(|projection| projection.at_epochs(epochs)),
                );
            }
            self.bootstrapped.store(true, Ordering::Release);
        }
        Ok([
            NativeHostProjectionKind::Snapshot,
            NativeHostProjectionKind::Replay,
        ]
        .to_vec())
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

    fn epochs(&self) -> NativeHostRuntimeEpochs {
        current_runtime_epochs(&self.epochs)
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

impl native_host_runtime_sealed::Sealed for NativeHostClientRuntime {}

impl Drop for NativeHostClientRuntime {
    fn drop(&mut self) {
        let deadline = NativeShutdownDeadline::from_now(NATIVE_SHUTDOWN_BUDGET);
        reap_retained_workers();
        reap_retained_children();
        self.cancellation.store(true, Ordering::Release);
        if self.worker.is_some() {
            // The bounded queue must never make shutdown wait behind pending
            // actions. The worker also observes the cancellation flag while
            // an in-flight async command is running.
            let _ = self.command_tx.try_send(NativeHostWorkerCommand::Shutdown);
        }
        if let Some(worker) = self.worker.take() {
            join_worker_with_deadline(worker, deadline);
        }
        if let Some(runtime) = self.runtime_guard.as_ref() {
            // A worker retained past the join budget may still own either
            // mutex while it observes cancellation. Never block on those
            // locks after the shared deadline has started; the retained
            // worker keeps the client/subscription ownership alive for its
            // eventual reaper.
            if let (Ok(mut client), Ok(mut subscription)) =
                (self.client.try_lock(), self.subscription.try_lock())
            {
                let remaining = deadline.remaining();
                if !remaining.is_zero() {
                    let _ = runtime.block_on(async {
                        tokio::time::timeout(remaining, subscription.release(&mut client)).await
                    });
                }
            }
        }
        if let Some(process) = self.host_process.as_mut() {
            let kind = std::mem::replace(&mut process.kind, NativeHostProcessKind::Empty);
            if let NativeHostProcessKind::Child(child) = kind {
                terminate_child_with_deadline(child, deadline);
            }
        }
        if let Some(runtime) = self.runtime_guard.take() {
            // The worker owns the only other runtime reference.  If it has
            // exited, take the runtime value and use Tokio's bounded shutdown
            // rather than allowing Runtime::drop to wait indefinitely.
            if let Ok(runtime) = Arc::try_unwrap(runtime) {
                runtime.shutdown_timeout(deadline.remaining());
            }
        }
    }
}

async fn connect_with_startup_retry(
    profile: &IsolatedDevProfile,
    deadline: NativeShutdownDeadline,
) -> Result<HostClient, IpcError> {
    let config = profile.host_client_config();
    let mut last_error = None;
    for _ in 0..40 {
        if deadline.expired() {
            return Err(last_error.unwrap_or(IpcError::Unavailable));
        }
        match tokio::time::timeout(deadline.remaining(), HostClient::connect(config.clone())).await
        {
            Ok(Ok(client)) => return Ok(client),
            Ok(Err(error)) => {
                last_error = Some(error);
            }
            Err(_) => return Err(last_error.unwrap_or(IpcError::Unavailable)),
        }
        if deadline.expired() {
            return Err(last_error.unwrap_or(IpcError::Unavailable));
        }
        match tokio::time::timeout(
            deadline.remaining(),
            tokio::time::sleep(Duration::from_millis(25)),
        )
        .await
        {
            Ok(()) => {}
            Err(_) => return Err(last_error.unwrap_or(IpcError::Unavailable)),
        }
    }
    Err(last_error.unwrap_or(IpcError::Unavailable))
}

fn native_host_worker_loop(
    client: Arc<Mutex<HostClient>>,
    subscription: Arc<Mutex<ClientSubscription>>,
    client_model: Arc<Mutex<Option<Arc<ClientModel>>>>,
    bootstrapped: Arc<AtomicBool>,
    runtime: Option<Arc<tokio::runtime::Runtime>>,
    cancellation: Arc<AtomicBool>,
    epochs: Arc<Mutex<NativeHostRuntimeEpochs>>,
    command_rx: Receiver<NativeHostWorkerCommand>,
    projections: Arc<Mutex<VecDeque<NativeHostProjection>>>,
) {
    let Some(runtime) = runtime else {
        return;
    };
    while !cancellation.load(Ordering::Acquire) {
        if !bootstrapped.load(Ordering::Acquire) {
            std::thread::sleep(CONTROLLER_TICK_INTERVAL);
            continue;
        }
        match command_rx.recv_timeout(CONTROLLER_TICK_INTERVAL) {
            Ok(NativeHostWorkerCommand::Shutdown) => break,
            Ok(NativeHostWorkerCommand::Execute(action)) => {
                if cancellation.load(Ordering::Acquire) {
                    break;
                }
                let projection = match action.command {
                    NativeHostCommand::Hold { action_id, reason } => NativeHostProjection {
                        kind: NativeHostProjectionKind::Error,
                        client_model: None,
                        error: Some(bounded_host_error(format!("{action_id}: HOLD: {reason}"))),
                        epochs: None,
                    },
                    command => {
                        let result = client.lock().ok().map(|mut client| {
                            runtime.block_on(execute_native_command_cancellable(
                                &mut client,
                                command,
                                &cancellation,
                            ))
                        });
                        match result {
                            Some(Ok(())) => {
                                NativeHostProjection::kind(NativeHostProjectionKind::Live)
                            }
                            Some(Err(error)) => NativeHostProjection {
                                kind: NativeHostProjectionKind::Error,
                                client_model: None,
                                error: Some(bounded_host_error(error.to_string())),
                                epochs: None,
                            },
                            None => NativeHostProjection {
                                kind: NativeHostProjectionKind::Error,
                                client_model: None,
                                error: Some("native host client lock poisoned".to_string()),
                                epochs: None,
                            },
                        }
                    }
                }
                .at_epochs(current_runtime_epochs(&epochs));
                if let Ok(mut queue) = projections.lock() {
                    if queue.len() < MAX_HOST_PROJECTIONS {
                        queue.push_back(projection);
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if cancellation.load(Ordering::Acquire) {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if cancellation.load(Ordering::Acquire) {
            break;
        }
        pump_subscription_once(
            &client,
            &subscription,
            &client_model,
            &runtime,
            &cancellation,
            &epochs,
            &projections,
        );
    }
}

fn pump_subscription_once(
    client: &Arc<Mutex<HostClient>>,
    subscription: &Arc<Mutex<ClientSubscription>>,
    client_model: &Arc<Mutex<Option<Arc<ClientModel>>>>,
    runtime: &tokio::runtime::Runtime,
    cancellation: &Arc<AtomicBool>,
    epochs: &Arc<Mutex<NativeHostRuntimeEpochs>>,
    projections: &Arc<Mutex<VecDeque<NativeHostProjection>>>,
) {
    if cancellation.load(Ordering::Acquire) {
        return;
    }
    let update = {
        let Ok(client_guard) = client.lock() else {
            publish_projection(
                projections,
                NativeHostProjection {
                    kind: NativeHostProjectionKind::Error,
                    client_model: None,
                    error: Some("native host client lock poisoned".to_string()),
                    epochs: None,
                }
                .at_epochs(current_runtime_epochs(epochs)),
            );
            return;
        };
        let Ok(mut subscription_guard) = subscription.lock() else {
            publish_projection(
                projections,
                NativeHostProjection {
                    kind: NativeHostProjectionKind::Error,
                    client_model: None,
                    error: Some("native host subscription lock poisoned".to_string()),
                    epochs: None,
                }
                .at_epochs(current_runtime_epochs(epochs)),
            );
            return;
        };
        runtime.block_on(tokio::time::timeout(
            Duration::from_millis(2),
            subscription_guard.recv_and_apply(&client_guard),
        ))
    };

    let update = match update {
        Ok(Ok(update)) => update,
        Ok(Err(error)) => {
            if matches!(
                error,
                crate::client::SubscriptionError::NeedsResync
                    | crate::client::SubscriptionError::Model(_)
                    | crate::client::SubscriptionError::ForeignSubscription(_)
                    | crate::client::SubscriptionError::InvalidResync
                    | crate::client::SubscriptionError::Transport(_)
                    | crate::client::SubscriptionError::Query(_)
                    | crate::client::SubscriptionError::IncompleteSnapshot
                    | crate::client::SubscriptionError::MissingCapabilities
            ) {
                let was_connected = client
                    .lock()
                    .map(|client| client.is_connected())
                    .unwrap_or(false);
                match resynchronize_subscription(
                    client,
                    subscription,
                    runtime,
                    cancellation,
                    NativeShutdownDeadline::from_now(NATIVE_STARTUP_BUDGET),
                ) {
                    Ok((model, reconnected)) => {
                        if reconnected || !was_connected {
                            bump_connection_epoch(epochs);
                        }
                        let current_epochs = bump_resource_generation(epochs);
                        if let Ok(mut current) = client_model.lock() {
                            *current = Some(Arc::clone(&model));
                        }
                        publish_projection(
                            projections,
                            NativeHostProjection::client_model(model).at_epochs(current_epochs),
                        );
                        publish_projection(
                            projections,
                            NativeHostProjection::kind(NativeHostProjectionKind::Replay)
                                .at_epochs(current_epochs),
                        );
                    }
                    Err(resync_error)
                        if resync_error != "native host subscription resync cancelled" =>
                    {
                        publish_projection(
                            projections,
                            NativeHostProjection {
                                kind: NativeHostProjectionKind::Error,
                                client_model: None,
                                error: Some(resync_error),
                                epochs: None,
                            }
                            .at_epochs(current_runtime_epochs(epochs)),
                        );
                    }
                    Err(_) => {}
                }
            }
            return;
        }
        Err(_) => return,
    };
    match update {
        SubscriptionUpdate::DurableEvent(_) => {
            let model = subscription
                .lock()
                .ok()
                .and_then(|subscription| subscription.model().cloned())
                .map(Arc::new);
            if let Some(model) = model {
                if let Ok(mut current) = client_model.lock() {
                    *current = Some(Arc::clone(&model));
                }
                publish_projection(
                    projections,
                    NativeHostProjection::model(NativeHostProjectionKind::Live, model)
                        .at_epochs(current_runtime_epochs(epochs)),
                );
            }
        }
        SubscriptionUpdate::ResyncRequired { .. } => {
            let was_connected = client
                .lock()
                .map(|client| client.is_connected())
                .unwrap_or(false);
            let model = resynchronize_subscription(
                client,
                subscription,
                runtime,
                cancellation,
                NativeShutdownDeadline::from_now(NATIVE_STARTUP_BUDGET),
            );
            match model {
                Ok((model, reconnected)) => {
                    if reconnected || !was_connected {
                        bump_connection_epoch(epochs);
                    }
                    let current_epochs = bump_resource_generation(epochs);
                    if let Ok(mut current) = client_model.lock() {
                        *current = Some(Arc::clone(&model));
                    }
                    publish_projection(
                        projections,
                        NativeHostProjection::client_model(model).at_epochs(current_epochs),
                    );
                    publish_projection(
                        projections,
                        NativeHostProjection::kind(NativeHostProjectionKind::Replay)
                            .at_epochs(current_epochs),
                    );
                }
                Err(error) => publish_projection(
                    projections,
                    NativeHostProjection {
                        kind: NativeHostProjectionKind::Error,
                        client_model: None,
                        error: Some(error),
                        epochs: None,
                    }
                    .at_epochs(current_runtime_epochs(epochs)),
                ),
            }
        }
        SubscriptionUpdate::Stream(_) => {
            publish_projection(
                projections,
                NativeHostProjection::kind(NativeHostProjectionKind::Live)
                    .at_epochs(current_runtime_epochs(epochs)),
            );
        }
    }
}

fn resynchronize_subscription(
    client: &Arc<Mutex<HostClient>>,
    subscription: &Arc<Mutex<ClientSubscription>>,
    runtime: &tokio::runtime::Runtime,
    cancellation: &Arc<AtomicBool>,
    deadline: NativeShutdownDeadline,
) -> Result<(Arc<ClientModel>, bool), String> {
    let mut client_guard = client
        .lock()
        .map_err(|_| "native host client lock poisoned".to_string())?;
    let mut subscription_guard = subscription
        .lock()
        .map_err(|_| "native host subscription lock poisoned".to_string())?;
    let was_connected = client_guard.is_connected();
    let result = runtime.block_on(async {
        tokio::time::timeout(deadline.remaining(), async {
            tokio::select! {
                result = async {
                    if client_guard.is_connected() {
                        subscription_guard
                            .release(&mut client_guard)
                            .await
                            .map_err(|error| error.to_string())?;
                    }
                    if !client_guard.is_connected() {
                        client_guard
                            .reconnect()
                            .await
                            .map_err(|error| error.to_string())?;
                    }
                    *subscription_guard = ClientSubscription::new();
                    subscription_guard
                        .synchronize(&mut client_guard)
                        .await
                        .map_err(|error| error.to_string())
                } => result,
                _ = wait_for_cancellation(Arc::clone(cancellation)) => {
                    Err("native host subscription resync cancelled".to_string())
                }
            }
        })
        .await
        .map_err(|_| "native host subscription resync deadline expired".to_string())?
    });
    result?;
    let model = subscription_guard
        .model()
        .cloned()
        .map(Arc::new)
        .ok_or_else(|| "native host resync produced no client model".to_string())?;
    Ok((model, !was_connected))
}

fn publish_projection(
    projections: &Arc<Mutex<VecDeque<NativeHostProjection>>>,
    projection: NativeHostProjection,
) {
    if let Ok(mut queue) = projections.lock() {
        if queue.len() < MAX_HOST_PROJECTIONS {
            queue.push_back(projection);
        }
    }
}

async fn execute_native_command(
    client: &mut HostClient,
    command: NativeHostCommand,
) -> Result<(), IpcError> {
    let envelope = match command {
        NativeHostCommand::Envelope(envelope) => envelope,
        NativeHostCommand::TaskCreate {
            arguments,
            command_id,
            issued_at_ms,
        } => crate::client::action::task_create_command(
            command_id,
            client.client_id(),
            issued_at_ms,
            arguments,
        )
        .map_err(|_| IpcError::Unavailable)?,
        NativeHostCommand::TaskRename {
            arguments,
            expected_task_revision,
            command_id,
            issued_at_ms,
        } => crate::client::action::task_rename_command(
            command_id,
            client.client_id(),
            issued_at_ms,
            expected_task_revision,
            arguments,
        )
        .map_err(|_| IpcError::Unavailable)?,
        NativeHostCommand::Hold { .. } => return Err(IpcError::Unavailable),
    };
    client.execute_command(envelope).await.map(|_| ())
}

async fn execute_native_command_cancellable(
    client: &mut HostClient,
    command: NativeHostCommand,
    cancellation: &Arc<AtomicBool>,
) -> Result<(), IpcError> {
    tokio::select! {
        result = execute_native_command(client, command) => result,
        _ = wait_for_cancellation(Arc::clone(cancellation)) => Err(IpcError::Unavailable),
    }
}

async fn wait_for_cancellation(cancellation: Arc<AtomicBool>) {
    while !cancellation.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn unix_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

pub(crate) enum NativeHostRuntimeAttachment {
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
    action_epoch: u64,
    connection_epoch: u64,
    client_epoch: u64,
    resource_generation: u64,
    runtime_generation: u64,
    client_model: Option<Arc<ClientModel>>,
    pointer_owner: Option<PointerOwner>,
    pointer_capture: Option<(u64, PointerButton)>,
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
            action_epoch: 0,
            connection_epoch: 0,
            client_epoch: 0,
            resource_generation: 0,
            runtime_generation: 0,
            client_model: None,
            pointer_owner: None,
            pointer_capture: None,
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

    pub fn action_epochs(&self) -> NativeActionEpochs {
        NativeActionEpochs {
            connection_epoch: self.connection_epoch,
            client_epoch: self.client_epoch,
            navigation_epoch: self.shell.navigation_epoch(),
            resource_generation: self.resource_generation,
            runtime_generation: self.runtime_generation,
            action_epoch: self.action_epoch,
            focus_epoch: self
                .last_handler
                .as_ref()
                .map(|trace| trace.focus_epoch)
                .unwrap_or_else(|| self.current_focus_epoch()),
        }
    }

    pub fn set_connection_epoch(&mut self, epoch: u64) {
        self.connection_epoch = epoch;
    }

    pub fn set_client_epoch(&mut self, epoch: u64) {
        self.client_epoch = epoch;
    }

    pub fn set_resource_generation(&mut self, generation: u64) {
        self.resource_generation = generation;
    }

    pub fn set_runtime_generation(&mut self, generation: u64) {
        self.runtime_generation = generation;
    }

    pub(crate) fn sync_host_epochs(&mut self, epochs: NativeHostRuntimeEpochs) -> bool {
        let changed = self.connection_epoch != epochs.connection_epoch
            || self.resource_generation != epochs.resource_generation
            || self.runtime_generation != epochs.runtime_generation;
        if changed {
            self.connection_epoch = epochs.connection_epoch;
            self.resource_generation = epochs.resource_generation;
            self.runtime_generation = epochs.runtime_generation;
            self.pending_keyboard = None;
            self.pointer_capture = None;
            self.pointer_owner = None;
        }
        changed
    }

    pub fn accepts_action_record(&self, record: &NativeActionRecord) -> bool {
        let epochs_match = record.connection_epoch == self.connection_epoch
            && record.client_epoch == self.client_epoch
            && record.navigation_epoch == self.shell.navigation_epoch()
            && record.resource_generation == self.resource_generation
            && record.runtime_generation == self.runtime_generation;
        let task_match = record.task_id.is_none_or(|task_id| {
            self.last_handler.as_ref().and_then(|trace| trace.task_id) == Some(task_id)
        }) && record.task_id.is_none_or(|task_id| {
            match record.expected_task_revision {
                None => true,
                Some(expected_revision) => self
                    .client_model
                    .as_ref()
                    .and_then(|model| model.tasks().get(&task_id))
                    .is_some_and(|task| {
                        expected_revision == task.task.revision
                            && record.captured_task_action_epoch == Some(task.task.action_epoch)
                    }),
            }
        });
        epochs_match
            && task_match
            && self.last_handler.as_ref().is_some_and(|trace| {
                trace.focus_epoch == record.focus_epoch
                    && trace.request_generation == record.request_generation
                    && trace.action_epoch == record.action_epoch
                    && trace.consumed
                    && trace.propagation_stopped
            })
    }

    /// Provide the immutable transport projection used to capture mutation
    /// fences. A task-only attachment deliberately clears this value so a
    /// rename cannot be synthesized with revision zero.
    pub fn set_client_model(&mut self, model: Option<Arc<ClientModel>>) {
        self.client_model = model;
        self.client_epoch = self.client_epoch.saturating_add(1);
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
        self.action_epoch = self
            .action_epoch
            .checked_add(1)
            .expect("native action epoch exhausted");
        self.focus_epochs.advance();
        self.last_handler = Some(HandlerTrace {
            focus_epoch,
            task_id,
            request_generation,
            action_epoch: self.action_epoch,
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
                self.pointer_capture = Some((pointer_id, button));
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

    pub fn terminal_mouse_up_for(
        &mut self,
        pointer_id: u64,
        button: PointerButton,
    ) -> TerminalReleaseOutcome {
        self.terminal_mouse_up_checked(pointer_id, Some(button))
    }

    /// Consume a release for a GPUI button that has no terminal mapping. It is
    /// deliberately not coerced to the primary button: an unsupported event
    /// must never authorize a different captured button.
    pub fn terminal_mouse_up_unmapped(&mut self, pointer_id: u64) -> TerminalReleaseOutcome {
        self.terminal_mouse_up_checked(pointer_id, None)
    }

    fn terminal_mouse_up_checked(
        &mut self,
        pointer_id: u64,
        button: Option<PointerButton>,
    ) -> TerminalReleaseOutcome {
        let task_id = self.selected_task();
        let (focus_epoch, request_generation) = self.begin_handler(task_id);
        let release = match (self.pointer_capture, self.pointer_owner.as_ref()) {
            (Some(capture), Some(_)) if button == Some(capture.1) && capture.0 == pointer_id => {
                self.pointer_capture = None;
                self.shell.terminal_mouse_up(self.pointer_owner.take())
            }
            (Some(_), _) => {
                TerminalRelease::Rejected(crate::ui::shell::ReleaseRejection::MismatchedOwner)
            }
            _ => {
                self.pointer_capture = None;
                self.pointer_owner.take();
                self.shell.terminal_mouse_up(None)
            }
        };
        TerminalReleaseOutcome {
            focus_epoch,
            task_id,
            request_generation,
            consumed: release.consumed(),
            propagation_stopped: true,
            release,
        }
    }

    /// Compatibility seam for callers that already hold the shell-owned
    /// native pointer id/button. New GPUI paths must use
    /// [`Self::terminal_mouse_up_for`] so the event button is checked.
    pub fn terminal_mouse_up(&mut self) -> TerminalReleaseOutcome {
        let (pointer_id, button) = self
            .pointer_capture
            .unwrap_or((NATIVE_POINTER_ID, PointerButton::Primary));
        self.terminal_mouse_up_for(pointer_id, button)
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
        let (expected_task_revision, captured_task_action_epoch) = match &request {
            ActionRequest::TaskRename(arguments) => {
                let model = self.client_model.as_ref()?;
                let task = model.tasks().get(&arguments.task_id)?;
                // A valid host mutation must always carry a real durable
                // revision. Revision zero is an invalid/unfenced sentinel.
                if task.task.revision == 0 {
                    return None;
                }
                (Some(task.task.revision), Some(task.task.action_epoch))
            }
            _ => (None, None),
        };
        let (focus_epoch, request_generation) = self.begin_handler(self.selected_task());
        let command_id = CommandId::new();
        let issued_at_ms = unix_time_ms();
        let command = match &request {
            ActionRequest::TaskCreate(arguments) => NativeHostCommand::TaskCreate {
                arguments: arguments.clone(),
                command_id,
                issued_at_ms,
            },
            ActionRequest::TaskRename(arguments) => NativeHostCommand::TaskRename {
                arguments: arguments.clone(),
                expected_task_revision: expected_task_revision
                    .expect("rename revision was validated above"),
                command_id,
                issued_at_ms,
            },
            ActionRequest::HostActions => NativeHostCommand::Hold {
                action_id: action::ACTION_HOST_ACTIONS,
                reason: "canonical host action catalog request is not wired",
            },
            ActionRequest::HostStatus => NativeHostCommand::Hold {
                action_id: action::ACTION_HOST_STATUS,
                reason: "canonical host status request is not wired",
            },
            ActionRequest::TaskList => NativeHostCommand::Hold {
                action_id: action::ACTION_TASK_LIST,
                reason: "canonical task list query request is not wired",
            },
            ActionRequest::TaskShow { .. } => NativeHostCommand::Hold {
                action_id: action::ACTION_TASK_SHOW,
                reason: "canonical task show query request is not wired",
            },
        };
        let event = ActionEvent::new(request, source, focus_epoch);
        Some(NativeActionRecord {
            id: descriptor.id,
            focus_epoch,
            request_generation,
            action_epoch: self.action_epoch,
            connection_epoch: self.connection_epoch,
            client_epoch: self.client_epoch,
            navigation_epoch: self.shell.navigation_epoch(),
            resource_generation: self.resource_generation,
            runtime_generation: self.runtime_generation,
            task_id: request_task,
            expected_task_revision,
            captured_task_action_epoch,
            capability: descriptor.required_capability,
            disabled_reason: None,
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
    task_node_ids: Vec<(accesskit::NodeId, TaskId)>,
}

impl AccessibilityTree {
    pub fn for_task_list(task_list: &TaskList, selected_task: Option<TaskId>) -> Self {
        let header = NativeHeaderAttachment::default();
        Self::for_task_list_with_header(task_list, selected_task, &header)
    }

    pub fn for_task_list_with_header(
        task_list: &TaskList,
        selected_task: Option<TaskId>,
        header: &NativeHeaderAttachment,
    ) -> Self {
        let rendered_task_ids = task_list.rendered_task_ids();
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
        .gpui("native-shell-toolbar", false, false)
        .with_children(vec![AccessibilityNode::new(
            AccessibleRole::Status,
            header.label(),
            header.detail(),
        )
        .gpui("native-shell-header-attachment", false, false)]);
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
            rendered_task_count: rendered_task_ids.len(),
            // `accesskit_tree_update` assigns IDs in pre-order. The shell's
            // root, toolbar, header, inbox, and inbox-status occupy 0..=4,
            // making the row mapping stable while the same tree is rendered.
            task_node_ids: rendered_task_ids
                .iter()
                .enumerate()
                .map(|(index, task_id)| (accesskit::NodeId::from(5 + index as u64), *task_id))
                .collect(),
        }
    }

    pub fn root(&self) -> &AccessibilityNode {
        &self.root
    }

    pub fn rendered_task_count(&self) -> usize {
        self.rendered_task_count
    }

    fn task_for_platform_node(&self, node_id: accesskit::NodeId) -> Option<TaskId> {
        self.task_node_ids
            .iter()
            .find_map(|(candidate, task_id)| (*candidate == node_id).then_some(*task_id))
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
    pending_actions: Arc<Mutex<VecDeque<NativeAccessibilityAction>>>,
    generation: Arc<std::sync::atomic::AtomicU64>,
    node_count: usize,
    attached: bool,
    #[cfg(windows)]
    adapter: Option<accesskit_windows::SubclassingAdapter>,
}

struct NativeAccessibilityAction {
    request: accesskit::ActionRequest,
    tree_generation: u64,
}

impl NativePlatformAccessibilityBridge {
    fn new(tree: &AccessibilityTree) -> Self {
        let update = accesskit_tree_update(tree);
        Self {
            tree: Arc::new(Mutex::new(update)),
            pending_actions: Arc::new(Mutex::new(VecDeque::new())),
            generation: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            node_count: tree.nodes().len(),
            attached: false,
            #[cfg(windows)]
            adapter: None,
        }
    }

    fn is_available(&self) -> bool {
        self.attached
    }

    fn take_actions(&mut self, max: usize) -> Vec<NativeAccessibilityAction> {
        self.pending_actions
            .lock()
            .map(|mut actions| {
                let count = max.min(MAX_ACCESSIBILITY_ACTIONS).min(actions.len());
                actions.drain(..count).collect()
            })
            .unwrap_or_default()
    }

    fn node_count(&self) -> usize {
        self.node_count
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn is_current_generation(&self, generation: u64) -> bool {
        generation == self.generation()
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
            let action_handler = NativeAccessKitActionHandler {
                pending: Arc::clone(&self.pending_actions),
                generation: Arc::clone(&self.generation),
            };
            self.adapter = Some(accesskit_windows::SubclassingAdapter::new(
                hwnd,
                activation,
                action_handler,
            ));
            self.attached = true;
        }
        #[cfg(not(windows))]
        let _ = window;
    }

    fn sync(&mut self, tree: &AccessibilityTree) {
        self.node_count = tree.nodes().len();
        let _ = self
            .generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation.checked_add(1)
            });
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
        if matches!(source.role(), AccessibleRole::Button) {
            // A Button role alone does not advertise invokable/focusable
            // semantics to every AccessKit consumer.  Keep these actions on
            // the same node that the GPUI row uses; no parallel action model
            // is introduced.
            node.add_action(accesskit::Action::Click);
            node.add_action(accesskit::Action::Focus);
        }
        let metadata = source.metadata();
        if metadata.disabled {
            node.set_disabled();
        }
        if metadata.busy {
            node.set_busy();
        }
        if metadata.read_only {
            node.set_read_only();
        }
        if metadata.invalid {
            node.set_invalid(accesskit::Invalid::Grammar);
        }
        if let Some(value) = metadata.value.as_ref() {
            node.set_value(value.clone());
        }
        if let Some(error) = metadata.error.as_ref() {
            node.set_description(format!("{} {}", source.description(), error));
        }
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
struct NativeAccessKitActionHandler {
    pending: Arc<Mutex<VecDeque<NativeAccessibilityAction>>>,
    generation: Arc<std::sync::atomic::AtomicU64>,
}

#[cfg(windows)]
impl accesskit::ActionHandler for NativeAccessKitActionHandler {
    fn do_action(&mut self, request: accesskit::ActionRequest) {
        if let Ok(mut pending) = self.pending.lock() {
            if pending.len() < MAX_ACCESSIBILITY_ACTIONS {
                pending.push_back(NativeAccessibilityAction {
                    request,
                    tree_generation: self.generation.load(Ordering::Acquire),
                });
            }
        }
    }
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
    header_attachment: NativeHeaderAttachment,
    client_model: Option<Arc<ClientModel>>,
    inbox: Inbox,
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

    pub(crate) fn new_with_host_runtime_port(
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
        let inbox = Inbox::empty();
        let header_attachment = NativeHeaderAttachment::default();
        let accessibility_tree = AccessibilityTree::for_task_list_with_header(
            inbox.task_list(),
            None,
            &header_attachment,
        );
        let platform_accessibility = NativePlatformAccessibilityBridge::new(&accessibility_tree);
        let mut interaction = NativeInteraction::new(None);
        let initial_epochs = host_runtime
            .as_ref()
            .map(|runtime| match runtime {
                NativeHostRuntimeAttachment::Injected(runtime) => runtime.epochs(),
                NativeHostRuntimeAttachment::Client(runtime) => runtime.epochs(),
            })
            .unwrap_or_default();
        interaction.sync_host_epochs(initial_epochs);
        let mut shell = Self {
            host_connection: profile.host_connection(),
            profile,
            host_runtime,
            host_state,
            preferences,
            header_attachment,
            client_model: None,
            inbox,
            interaction,
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

    pub fn header_attachment(&self) -> &NativeHeaderAttachment {
        &self.header_attachment
    }

    /// Attach a bounded immutable header projection supplied by the canonical
    /// runtime. This is an adapter-only mutation; it never touches transport.
    pub fn attach_header_projection(&mut self, attachment: NativeHeaderAttachment) {
        self.header_attachment = attachment;
        self.accessibility_tree = AccessibilityTree::for_task_list_with_header(
            self.inbox.task_list(),
            self.interaction.selected_task(),
            &self.header_attachment,
        );
        self.platform_accessibility.sync(&self.accessibility_tree);
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
        enqueue_pending_preference(&mut self.pending_preferences, preferences);
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

        let runtime_epochs = self
            .host_runtime
            .as_ref()
            .map(|runtime| match runtime {
                NativeHostRuntimeAttachment::Injected(runtime) => runtime.epochs(),
                NativeHostRuntimeAttachment::Client(runtime) => runtime.epochs(),
            })
            .unwrap_or_default();
        self.interaction.sync_host_epochs(runtime_epochs);

        let projections = match self.host_runtime.as_mut() {
            Some(NativeHostRuntimeAttachment::Injected(runtime)) => {
                runtime.drain_projection_messages(max)
            }
            Some(NativeHostRuntimeAttachment::Client(runtime)) => {
                runtime.take_ready_projection_messages(max)
            }
            None => Vec::new(),
        };
        let had_projections = !projections.is_empty();
        let mut accepted_projection_kinds = Vec::with_capacity(projections.len());
        for projection in projections {
            if projection
                .epochs
                .is_some_and(|epochs| epochs != runtime_epochs)
            {
                continue;
            }
            accepted_projection_kinds.push(projection.kind);
            if let Some(model) = projection.client_model {
                if let Err(error) = self.apply_client_model(model) {
                    self.host_state = NativeHostState::Error { message: error };
                }
            }
            if let Some(error) = projection.error {
                self.host_state = NativeHostState::Error { message: error };
            }
        }
        if had_projections {
            self.last_projection_kinds = accepted_projection_kinds;
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

        // AccessKit invokes its OS action handler from the platform thread.
        // The handler only queues bounded requests; this controller tick is
        // the sole GPUI/input owner that resolves them against the current
        // rendered tree and focus capture.
        for queued in self.platform_accessibility.take_actions(max) {
            if !self
                .platform_accessibility
                .is_current_generation(queued.tree_generation)
            {
                // Node ids are only meaningful within the tree generation
                // that produced them.  A reorder/removal must never route an
                // old assistive-technology action to a new row.
                continue;
            }
            let request = queued.request;
            if !matches!(
                request.action,
                accesskit::Action::Click | accesskit::Action::Focus
            ) {
                continue;
            }
            let Some(task_id) = self
                .accessibility_tree
                .task_for_platform_node(request.target_node)
            else {
                continue;
            };
            let _ = self
                .interaction
                .navigation_mouse_down(task_id, self.inbox.task_list());
        }

        let offset = self.task_scroll_handle.0.borrow().base_handle.offset().y / px(1.0);
        let metrics = self.preferences.tokens().density.physical();
        let _ = self.inbox.task_list_mut().set_scroll_offset_pixels(
            -offset,
            metrics.row_height as f32 * DEFAULT_VISIBLE_ROWS as f32,
            metrics.row_height as f32,
        );
        self.accessibility_tree = AccessibilityTree::for_task_list_with_header(
            self.inbox.task_list(),
            self.interaction.selected_task(),
            &self.header_attachment,
        );
        self.platform_accessibility.sync(&self.accessibility_tree);
    }

    pub(crate) fn install_window_observers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        self.interaction.sync_host_epochs(host_runtime.epochs());
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
    fn apply_task_list(&mut self, task_list: TaskList) {
        self.client_model = None;
        self.interaction.set_client_model(None);
        let selected_task = self
            .interaction
            .selected_task()
            .filter(|task_id| task_list.task_ids().contains(task_id));
        self.interaction.sync_selected_task(selected_task);
        self.inbox = Inbox::from_task_list(task_list);
        self.accessibility_tree = AccessibilityTree::for_task_list_with_header(
            self.inbox.task_list(),
            self.interaction.selected_task(),
            &self.header_attachment,
        );
        self.platform_accessibility.sync(&self.accessibility_tree);
    }

    pub fn apply_client_model(&mut self, model: Arc<ClientModel>) -> Result<(), String> {
        let task_list = TaskList::from_client_model_virtual(&model)
            .map_err(|error| format!("client model task projection failed: {error:?}"))?;
        self.apply_task_list(task_list);
        self.client_model = Some(Arc::clone(&model));
        self.interaction.set_client_model(Some(model));
        Ok(())
    }

    pub fn client_model_snapshot(&self) -> Option<Arc<ClientModel>> {
        self.client_model.clone()
    }

    pub fn task_list(&self) -> &TaskList {
        self.inbox.task_list()
    }

    pub fn inbox(&self) -> &Inbox {
        &self.inbox
    }

    pub fn rendered_task_count(&self) -> usize {
        self.inbox
            .task_list()
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
            .flex_wrap()
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
                    .id("native-shell-header-attachment")
                    .text_color(tokens.text.secondary.to_gpui())
                    .whitespace_normal()
                    .child(self.header_attachment.label()),
            )
            .child(
                div()
                    .id("native-shell-header-detail")
                    .text_color(tokens.text.secondary.to_gpui())
                    .whitespace_normal()
                    .child(self.header_attachment.detail()),
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
        let task_ids = self.inbox.task_list().shared_task_ids();
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
                        let shell_for_mouse_up = shell_entity.clone();
                        let shell_for_key = shell_entity.clone();
                        let mouse_handler =
                            move |event: &MouseDownEvent,
                                  window: &mut Window,
                                  app: &mut gpui::App| {
                                let _ = shell_for_mouse.update(app, |shell, cx| {
                                    cx.stop_propagation();
                                    if event.button == MouseButton::Left {
                                        shell.focus_handle.focus(window);
                                        let _ = shell.interaction.navigation_mouse_down(
                                            task_id,
                                            shell.inbox.task_list(),
                                        );
                                        shell.accessibility_tree =
                                            AccessibilityTree::for_task_list_with_header(
                                                shell.inbox.task_list(),
                                                shell.interaction.selected_task(),
                                                &shell.header_attachment,
                                            );
                                        shell
                                            .platform_accessibility
                                            .sync(&shell.accessibility_tree);
                                    }
                                });
                            };
                        let mouse_up_handler =
                            move |_event: &MouseUpEvent,
                                  _window: &mut Window,
                                  app: &mut gpui::App| {
                                let _ = shell_for_mouse_up.update(app, |_shell, cx| {
                                    cx.stop_propagation();
                                });
                            };
                        let key_handler =
                            move |event: &KeyDownEvent,
                                  _window: &mut Window,
                                  app: &mut gpui::App| {
                                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                    let _ = shell_for_key.update(app, |shell, cx| {
                                        cx.stop_propagation();
                                        let _ = shell.interaction.navigation_mouse_down(
                                            task_id,
                                            shell.inbox.task_list(),
                                        );
                                        shell.accessibility_tree =
                                            AccessibilityTree::for_task_list_with_header(
                                                shell.inbox.task_list(),
                                                shell.interaction.selected_task(),
                                                &shell.header_attachment,
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
                            // Capture every button at the row boundary so a
                            // right/middle click cannot fall through to the
                            // terminal dock behind the list. The handler
                            // still navigates only on the explicit primary
                            // button.
                            .capture_any_mouse_down(mouse_handler)
                            .capture_any_mouse_up(mouse_up_handler)
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
            if let (Some(task_id), Some(button)) = (selected, pointer_button(event.button)) {
                shell.interaction.terminal_mouse_down(
                    NATIVE_POINTER_ID,
                    task_id,
                    button,
                    Some(task_id),
                );
            }
        });
        let terminal_up = cx.listener(|shell, event: &MouseUpEvent, _window, cx| {
            cx.stop_propagation();
            if let Some(button) = pointer_button(event.button) {
                shell
                    .interaction
                    .terminal_mouse_up_for(NATIVE_POINTER_ID, button);
            } else {
                shell
                    .interaction
                    .terminal_mouse_up_unmapped(NATIVE_POINTER_ID);
            }
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
            shell.accessibility_tree = AccessibilityTree::for_task_list_with_header(
                shell.inbox.task_list(),
                shell.interaction.selected_task(),
                &shell.header_attachment,
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
                    .flex_wrap()
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
                            .id("native-shell-header-attachment")
                            .text_color(tokens.text.secondary.to_gpui())
                            .whitespace_normal()
                            .child(self.header_attachment.label()),
                    )
                    .child(
                        div()
                            .id("native-shell-header-detail")
                            .text_color(tokens.text.secondary.to_gpui())
                            .whitespace_normal()
                            .child(self.header_attachment.detail()),
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

pub(crate) fn run_native_shell_with_bootstrap(
    profile: IsolatedDevProfile,
    bootstrap: &mut dyn NativeHostBootstrap,
) -> Result<(), NativeShellError> {
    let deadline = Instant::now() + NATIVE_STARTUP_BUDGET;
    let (host_runtime, host_state) = match bootstrap.start_until(&profile, deadline) {
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

fn enqueue_pending_preference(
    pending: &mut VecDeque<RuntimePreferencesSnapshot>,
    preferences: RuntimePreferencesSnapshot,
) {
    if pending.len() >= MAX_PENDING_PREFERENCES {
        pending.pop_front();
    }
    pending.push_back(preferences);
}

#[cfg(test)]
mod tests {
    use super::{
        acquire_reaper_permit, enqueue_pending_preference, ensure_isolated_host_config_base,
        isolated_dev_profile, reap_retained_children, reap_retained_workers, retain_child,
        retain_worker, wait_for_cancellation, AccessibilityTree, NativePlatformAccessibilityBridge,
        NativeShutdownDeadline, OwnedChild, OwnedWorker, ReaperKind, MAX_PENDING_PREFERENCES,
        MAX_RETAINED_CHILDREN, MAX_RETAINED_WORKERS,
    };
    use std::collections::VecDeque;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn exited_child() -> std::process::Child {
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd");
            command.args(["/C", "exit", "0"]);
            command
        };
        #[cfg(not(windows))]
        let mut command = Command::new("true");
        let mut child = command.spawn().expect("short-lived test child");
        loop {
            match child.try_wait().expect("test child status") {
                Some(_) => return child,
                None => std::thread::yield_now(),
            }
        }
    }

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

    #[test]
    fn native_worker_cancellation_wait_observes_shutdown_without_a_long_sleep() {
        let cancellation = Arc::new(AtomicBool::new(false));
        let cancellation_for_thread = Arc::clone(&cancellation);
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            cancellation_for_thread.store(true, Ordering::Release);
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime");
        let started = Instant::now();
        runtime.block_on(wait_for_cancellation(cancellation));
        setter.join().expect("cancellation setter");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn native_shutdown_deadline_is_one_absolute_budget() {
        let deadline = NativeShutdownDeadline::from_now(Duration::from_millis(25));
        let first = deadline.remaining();
        std::thread::sleep(Duration::from_millis(3));
        let second = deadline.remaining();
        assert!(second < first);
        assert!(!deadline.expired());
        std::thread::sleep(Duration::from_millis(30));
        assert!(deadline.expired());
        assert_eq!(deadline.remaining(), Duration::ZERO);
    }

    #[test]
    fn accessibility_actions_from_an_old_tree_generation_are_not_current() {
        let tree =
            AccessibilityTree::for_task_list(&crate::ui::task_cockpit::TaskList::empty(), None);
        let mut bridge = NativePlatformAccessibilityBridge::new(&tree);
        let old_generation = bridge.generation();
        bridge.sync(&tree);
        assert!(!bridge.is_current_generation(old_generation));
        assert!(bridge.is_current_generation(bridge.generation()));
    }

    #[test]
    fn worker_reaper_capacity_recovers_finished_handles_before_admission() {
        reap_retained_workers();
        for _ in 0..MAX_RETAINED_WORKERS {
            let permit = acquire_reaper_permit(ReaperKind::Worker).expect("worker permit");
            let handle = std::thread::spawn(|| {});
            while !handle.is_finished() {
                std::thread::yield_now();
            }
            retain_worker(OwnedWorker {
                handle,
                _permit: permit,
            });
        }
        let recovered = acquire_reaper_permit(ReaperKind::Worker);
        assert!(
            recovered.is_some(),
            "finished workers must be reaped before cap"
        );
        drop(recovered);
        reap_retained_workers();
    }

    #[test]
    fn child_reaper_capacity_recovers_finished_handles_before_admission() {
        reap_retained_children();
        for _ in 0..MAX_RETAINED_CHILDREN {
            let permit = acquire_reaper_permit(ReaperKind::Child).expect("child permit");
            retain_child(OwnedChild {
                child: exited_child(),
                _permit: permit,
            });
        }
        let recovered = acquire_reaper_permit(ReaperKind::Child);
        assert!(
            recovered.is_some(),
            "finished children must be reaped before cap"
        );
        drop(recovered);
        reap_retained_children();
    }

    #[test]
    fn pending_preferences_keep_a_bounded_recent_window() {
        let mut pending = VecDeque::new();
        for _ in 0..(MAX_PENDING_PREFERENCES * 4) {
            enqueue_pending_preference(
                &mut pending,
                crate::ui::tokens::RuntimePreferencesSnapshot::default(),
            );
        }
        assert!(pending.len() <= MAX_PENDING_PREFERENCES);
    }
}

#[allow(dead_code)]
fn _terminal_adapter_dependency() -> &'static str {
    TERMINAL_ADAPTER_DEPENDENCY
}
