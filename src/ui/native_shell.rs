//! The real native Task Cockpit shell entrypoint and its isolated host seam.
//!
//! This module deliberately owns only a local projection. It does not open the
//! installed profile, read the production session, or start a legacy app. The
//! active shell talks to the one host-owned WebView authority through the
//! controller/lease seam below; the host runtime supplies the immutable
//! `ClientModel`, while the task cockpit and terminal adapter consume that
//! projection.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::env;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
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
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::assets::AppAssets;
use crate::browser::{
    BrowserError, BrowserNativeHostCommand, BrowserNativeHostOutcome, BrowserResponse,
    BrowserWebViewHost,
};
use crate::client::action::{self, BrowserActionRequest, CockpitSurfaceKind, UpdaterAction};
use crate::client::{
    ClientModel, ClientSubscription, HostClient, HostClientConfig, HostClientConnectPort,
    SubscriptionUpdate,
};
use crate::config::paths::{resolve_app_paths, AppProfile, BuildKind};
use crate::domain::cockpit::TaskCockpitQuery;
use crate::domain::host::{HostQuitInspection, HostQuitWorktreeInspection};
use crate::domain::id::SubscriptionId;
use crate::domain::id::{CommandId, RequestId, TaskId};
use crate::domain::snapshot::SnapshotSection;
use crate::domain::task::VisibleTaskStatus;
use crate::domain::ClientId;
use crate::host::IpcError;
use crate::prompts::projection::{PromptLibraryQuery, PromptNamespace, PromptProjectionReply};
use crate::protocol::BrowserSecurityState;
use crate::protocol::StreamFrame;
use crate::protocol::{Capability, CapabilitySet, FrameLimits};
use crate::remote::RemoteHostService;
use crate::ui::actions::{
    self, DockTool, HostActions, HostStatus, KeyboardAction, KeyboardModel, KeyboardShortcut,
    NativeDismissTransient, NativeDockArtifacts, NativeDockBrowser, NativeDockChanges,
    NativeDockFiles, NativeDockReview, NativeDockServices, NativeDockTerminal,
    NativeOpenCommandPalette, NativeOpenPalette, NativeOpenTaskSwitcher, NativeOpenTerminal,
    TaskCreate, TaskListAction, TaskRename, TaskShow,
};
use crate::ui::components::interaction::{FocusEpoch, FocusEpochSource};
use crate::ui::components::status_light::{ExternalPortStatus, StatusLight};
use crate::ui::components::{
    AccessibilityMetadata, AccessibleRole, ActionEvent, ActionRequest, ActivationSource,
    InteractionStateModel,
};
use crate::ui::prompts::mutation::apply_host_reply_to_session;
use crate::ui::prompts::{PromptLibraryKey, PromptLibrarySession};
use crate::ui::shell::{
    ColorScheme, DataFixtureKind, Density, LayoutWidth, NavigationResult, PointerButton,
    PointerOwner, PromptLibraryViewport, ScalePercent, Shell, TerminalPressRejection,
    TerminalRelease,
};
use crate::ui::task_cockpit::composer::{ComposerError, TaskComposer};
use crate::ui::task_cockpit::dock::{DockEdge, DockTool as CockpitDockTool};
use crate::ui::task_cockpit::shell::TaskCockpitShell;
use crate::ui::task_cockpit::{
    one_fresh_quota_observations, project_services_from_task_projection, project_services_panel,
    render_task_browser_dock, summary_line, update_observation_from_snapshot, Inbox,
    InboxPresentationWidth, InboxRenderModel, ServicePanelAction, ServicePanelTone,
    ServicesPanelProjection, TaskBrowserDockModel, TaskHeaderModel, TaskList,
    TopBarProjectionController, TopBarProjectionInput, UpdateState, DEFAULT_VISIBLE_ROWS,
    FIXED_VIRTUAL_OVERSCAN,
};
use crate::ui::terminal_adapter::TerminalDockAdapter;
pub use crate::ui::terminal_adapter::{TerminalDockState, TERMINAL_ADAPTER_DEPENDENCY};
use crate::ui::tokens::{RuntimePreferencesSnapshot, StatusMeaning};
use crate::updater::{UpdaterService, UpdaterSnapshot, UpdaterStage};

/// Explicit UI action: acknowledged client detach (host survives in production).
#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.client_detach")]
pub struct NativeClientDetach;

/// Explicit UI action: full host quit via inspect_host_quit + confirm_host_quit.
#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.host_full_quit")]
pub struct NativeHostFullQuit;

const NATIVE_PROFILE_DIR: &str = ".devmanager-next/dev-profile";
const NATIVE_PROFILE_NAME_PREFIX: &str = "native-next";
const NATIVE_HOST_SCHEME: &str = "devtest";
/// Stable pipe/lock profile name for the packaged production host.
const PRODUCTION_HOST_PROFILE: &str = "production";
const CLIENT_BUILD_PREFIX: &str = "devmanager";
const NATIVE_POINTER_ID: u64 = 1;
const MAX_RENDERED_TASK_ROWS: usize = DEFAULT_VISIBLE_ROWS + FIXED_VIRTUAL_OVERSCAN * 2;
const MAX_PENDING_HOST_ACTIONS: usize = 32;
const MAX_HOST_PROJECTIONS: usize = 64;
// One bounded action lane covers both transient runtime admission and shell
// retry ownership. Keeping the cap explicit prevents a full outcome queue and
// a full retry queue from silently multiplying memory or dropping identity.
const MAX_ACTION_LANE_RECORDS: usize = MAX_PENDING_HOST_ACTIONS * 2;
const MAX_ACTION_OUTCOME_PROJECTIONS: usize = MAX_ACTION_LANE_RECORDS;
const MAX_HOST_PROJECTION_MESSAGES: usize = MAX_HOST_PROJECTIONS + MAX_ACTION_OUTCOME_PROJECTIONS;
// Reserve one slot in the combined lane for an action outcome that arrives
// while the ordinary retry deque is full. This keeps every accepted Execute
// record durably owned without allowing outcome and retry queues to multiply
// the action bound.
const MAX_RETRY_HOST_ACTIONS: usize = MAX_ACTION_LANE_RECORDS - 1;
const MAX_ACCESSIBILITY_ACTIONS: usize = 32;
const MAX_PENDING_PREFERENCES: usize = 8;
const CONTROLLER_TICK_INTERVAL: Duration = Duration::from_millis(16);
const NATIVE_SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);
const NATIVE_STARTUP_BUDGET: Duration = Duration::from_secs(5);
const MAX_RETAINED_WORKERS: usize = 8;
const MAX_RETAINED_CHILDREN: usize = 8;
const MAX_RETAINED_ACTION_BATCHES: usize = 8;

fn action_lane_total(
    channel_count: usize,
    pending_count: usize,
    shell_retained_count: usize,
) -> usize {
    channel_count
        .saturating_add(pending_count)
        .saturating_add(shell_retained_count)
}

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
    ActionBatch,
}

#[derive(Debug, Default)]
struct ReaperCounts {
    workers: usize,
    children: usize,
    action_batches: usize,
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
        ReaperKind::ActionBatch => reap_retained_action_batches(),
    }
    let mut counts = reaper_counts()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (count, limit) = match kind {
        ReaperKind::Worker => (&mut counts.workers, MAX_RETAINED_WORKERS),
        ReaperKind::Child => (&mut counts.children, MAX_RETAINED_CHILDREN),
        ReaperKind::ActionBatch => (&mut counts.action_batches, MAX_RETAINED_ACTION_BATCHES),
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
            ReaperKind::ActionBatch => {
                counts.action_batches = counts.action_batches.saturating_sub(1)
            }
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

struct OwnedActionBatch {
    outcomes: VecDeque<NativeHostActionOutcome>,
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

fn retained_action_batches() -> &'static Mutex<VecDeque<OwnedActionBatch>> {
    static RETAINED: OnceLock<Mutex<VecDeque<OwnedActionBatch>>> = OnceLock::new();
    RETAINED.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn retained_action_emergency() -> &'static Mutex<VecDeque<NativeHostActionOutcome>> {
    static RETAINED: OnceLock<Mutex<VecDeque<NativeHostActionOutcome>>> = OnceLock::new();
    RETAINED.get_or_init(|| Mutex::new(VecDeque::new()))
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
        Ok(Some(_)) => false,
        Ok(None) | Err(_) => true,
    });
}

fn reap_retained_action_batches() {
    let mut batches = retained_action_batches()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    batches.retain(|batch| !batch.outcomes.is_empty());
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

fn retain_action_batch(outcomes: VecDeque<NativeHostActionOutcome>, permit: ReaperPermit) {
    if outcomes.is_empty() {
        drop(permit);
        return;
    }
    let mut batches = retained_action_batches()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if batches.len() >= MAX_RETAINED_ACTION_BATCHES {
        drop(batches);
        drop(permit);
        for outcome in outcomes {
            let _ = retain_emergency_action_outcome(outcome);
        }
        return;
    }
    batches.push_back(OwnedActionBatch {
        outcomes,
        _permit: permit,
    });
}

fn retain_emergency_action_outcome(outcome: NativeHostActionOutcome) -> bool {
    let mut retained = retained_action_emergency()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if retained.len() >= MAX_ACTION_LANE_RECORDS * MAX_RETAINED_ACTION_BATCHES {
        return false;
    }
    retained.push_back(outcome);
    true
}

pub(crate) fn take_retained_action_outcomes(max: usize) -> Vec<NativeHostActionOutcome> {
    let limit = max.min(MAX_ACTION_LANE_RECORDS);
    let mut outcomes = Vec::with_capacity(limit);
    let mut emergency = retained_action_emergency()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while outcomes.len() < limit {
        let Some(outcome) = emergency.pop_front() else {
            break;
        };
        outcomes.push(outcome);
    }
    drop(emergency);
    if outcomes.len() >= limit {
        return outcomes;
    }
    let mut batches = retained_action_batches()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while outcomes.len() < limit {
        let Some(mut batch) = batches.pop_front() else {
            break;
        };
        while outcomes.len() < limit {
            let Some(outcome) = batch.outcomes.pop_front() else {
                break;
            };
            outcomes.push(outcome);
        }
        if !batch.outcomes.is_empty() {
            batches.push_front(batch);
            break;
        }
    }
    outcomes
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
    if child.child.kill().is_err() {
        match child.child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) | Err(_) => {
                retain_child(child);
                return;
            }
        }
    }
    while !deadline.expired() {
        match child.child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(2).min(deadline.remaining())),
            Err(_) => {
                retain_child(child);
                return;
            }
        }
    }
    retain_child(child);
}

/// Wait for a host that has accepted an intentional full-quit request to
/// complete its durable cleanup before releasing the child handle. The host
/// owns the cleanup state machine; killing it immediately after the Accepted
/// receipt could strand the Closing journal. If it outlives the bounded wait,
/// retain the handle for the existing child reaper (the isolated host also has
/// its parent-pid watchdog).
fn wait_for_child_exit_with_deadline(mut child: OwnedChild, deadline: NativeShutdownDeadline) {
    reap_retained_children();
    while !deadline.expired() {
        match child.child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(2).min(deadline.remaining())),
            Err(_) => {
                retain_child(child);
                return;
            }
        }
    }
    retain_child(child);
}

fn stable_task_element_id(task_id: TaskId) -> ElementId {
    // GPUI has a UUID element identity, so retain all 128 bits of the domain
    // TaskId instead of collapsing it to an offset or a lossy hash.
    ElementId::Uuid(Uuid::from_bytes(*task_id.as_bytes()))
}

/// Return a deterministic numeric suffix for a service element identity.
///
/// GPUI's tuple element IDs intentionally accept only a static name and a
/// typed integer; a runtime `String` cannot be passed directly to `.id(...)`.
/// Hashing the validated service ID (and optional action label) keeps the
/// identity stable across renders without depending on row order, while the
/// static tuple name keeps rows and controls in separate ID namespaces.
fn stable_service_element_key(service_id: &str, suffix: &str) -> u64 {
    let mut digest = Sha256::new();
    digest.update(b"native-service");
    digest.update([0]);
    digest.update(service_id.as_bytes());
    digest.update([0]);
    digest.update(suffix.as_bytes());
    let digest = digest.finalize();
    u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("sha256 digest always contains eight bytes"),
    )
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

/// Whether the shell owns an isolated debug host or the durable production host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeShellMode {
    IsolatedDebug,
    Production,
}

/// Child-host lifetime policy for the shell that launched it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeHostChildOwnership {
    /// Debug/test: kill and reap the child when the client drops.
    TerminateWithClient,
    /// Production: client close detaches only; the durable host survives.
    DetachOnClientClose,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IsolatedDevProfile {
    workspace_root: PathBuf,
    root: PathBuf,
    named_profile: String,
    mode: NativeShellMode,
}

impl IsolatedDevProfile {
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn mode(&self) -> NativeShellMode {
        self.mode
    }

    pub fn is_production(&self) -> bool {
        self.mode == NativeShellMode::Production
    }

    /// Config base to pass to the isolated `devmanager-host` process. The
    /// host's named profile is created beneath this generated directory; it
    /// never resolves the installed app-data root.
    pub fn host_config_base(&self) -> &Path {
        &self.root
    }

    /// Pipe/lock profile name for host attachment.
    ///
    /// Isolated debug names are workspace-bound. Production uses the stable
    /// `production` profile and never reads `DEVMANAGER_PROFILE`.
    pub fn named_profile(&self) -> &str {
        &self.named_profile
    }

    fn child_ownership(&self) -> NativeHostChildOwnership {
        match self.mode {
            NativeShellMode::IsolatedDebug => NativeHostChildOwnership::TerminateWithClient,
            NativeShellMode::Production => NativeHostChildOwnership::DetachOnClientClose,
        }
    }

    /// Build the one client configuration used by the native shell.
    ///
    /// This does not connect, read a profile, or create files. A caller-owned
    /// host controller may use it from its I/O lane and then attach the single
    /// resulting [`HostClient`] through [`NativeHostClientRuntime`].
    pub fn host_client_config(&self) -> HostClientConfig {
        HostClientConfig {
            named_profile: self.named_profile().to_string(),
            client_build: format!("{CLIENT_BUILD_PREFIX}/{}", env!("CARGO_PKG_VERSION")),
            client_id: ClientId::new(),
            requested: CapabilitySet::from_capabilities([
                Capability::PagedSnapshots,
                Capability::EventReplay,
                Capability::ExplicitDetach,
                Capability::HostShutdown,
                Capability::ServiceSupervisor,
            ]),
            limits: FrameLimits::v1_default(),
        }
    }

    pub fn host_connection(&self) -> DevTestHostConnection {
        DevTestHostConnection {
            profile_root: self.root.clone(),
            endpoint: format!("{NATIVE_HOST_SCHEME}://{}", self.named_profile()),
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

/// Exact command line used to own or attach the sibling `devmanager-host`.
/// Keeping this as a value object makes the process seam injectable without
/// allowing tests to start an installed host from an isolated profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeHostLaunchSpec {
    executable: PathBuf,
    mode: NativeHostLaunchMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NativeHostLaunchMode {
    Isolated {
        profile: String,
        instance_label: String,
        parent_pid: u32,
        config_base: PathBuf,
    },
    Production,
}

impl NativeHostLaunchSpec {
    pub(crate) fn for_isolated_profile(
        profile: &IsolatedDevProfile,
        parent_pid: u32,
    ) -> Result<Self, NativeShellError> {
        if profile.is_production() {
            return Err(NativeShellError::HostConnect {
                message: "isolated host launch requires an isolated debug profile".to_string(),
            });
        }
        if parent_pid == 0 {
            return Err(NativeShellError::HostConnect {
                message: "native host parent PID must be nonzero".to_string(),
            });
        }
        Ok(Self {
            executable: native_host_executable()?,
            mode: NativeHostLaunchMode::Isolated {
                profile: profile.named_profile().to_string(),
                instance_label: "Native Debug".to_string(),
                parent_pid,
                config_base: profile.host_config_base().to_path_buf(),
            },
        })
    }

    pub(crate) fn for_production() -> Result<Self, NativeShellError> {
        Ok(Self {
            executable: native_host_executable()?,
            mode: NativeHostLaunchMode::Production,
        })
    }

    fn arguments(&self) -> Vec<String> {
        match &self.mode {
            NativeHostLaunchMode::Isolated {
                profile,
                instance_label,
                parent_pid,
                config_base,
            } => vec![
                "--profile".to_string(),
                profile.clone(),
                "--instance-label".to_string(),
                instance_label.clone(),
                "--parent-pid".to_string(),
                parent_pid.to_string(),
                "--foreground".to_string(),
                "--config-base".to_string(),
                config_base.display().to_string(),
            ],
            NativeHostLaunchMode::Production => vec!["--foreground".to_string()],
        }
    }
}

fn native_host_executable() -> Result<PathBuf, NativeShellError> {
    let current = std::env::current_exe().map_err(|error| NativeShellError::HostConnect {
        message: format!("native shell executable identity unavailable: {error}"),
    })?;
    let parent = current
        .parent()
        .ok_or_else(|| NativeShellError::HostConnect {
            message: "native shell executable has no parent directory".to_string(),
        })?;
    let sibling = parent.join(if cfg!(windows) {
        "devmanager-host.exe"
    } else {
        "devmanager-host"
    });
    validate_native_host_executable(&sibling)
}

fn validate_native_host_executable(path: &Path) -> Result<PathBuf, NativeShellError> {
    let parent = path.parent().ok_or_else(|| NativeShellError::HostConnect {
        message: format!(
            "isolated native host executable has no parent: {}",
            path.display()
        ),
    })?;
    let parent_metadata =
        std::fs::symlink_metadata(parent).map_err(|error| NativeShellError::HostConnect {
            message: format!("isolated native host executable parent is unavailable: {error}"),
        })?;
    if !parent_metadata.is_dir() || path_is_reparse_or_symlink(&parent_metadata) {
        return Err(NativeShellError::HostConnect {
            message: format!(
                "isolated native host executable parent is redirected: {}",
                parent.display()
            ),
        });
    }
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| NativeShellError::HostConnect {
            message: format!("isolated native host executable is unavailable: {error}"),
        })?;
    if !metadata.is_file() || path_is_reparse_or_symlink(&metadata) {
        return Err(NativeShellError::HostConnect {
            message: format!(
                "isolated native host executable must be a real file: {}",
                path.display()
            ),
        });
    }
    let canonical_parent =
        parent
            .canonicalize()
            .map_err(|error| NativeShellError::HostConnect {
                message: format!(
                    "isolated native host executable parent cannot be pinned: {error}"
                ),
            })?;
    let canonical = path
        .canonicalize()
        .map_err(|error| NativeShellError::HostConnect {
            message: format!("isolated native host executable cannot be pinned: {error}"),
        })?;
    if canonical.parent() != Some(canonical_parent.as_path()) {
        return Err(NativeShellError::HostConnect {
            message: format!(
                "isolated native host executable redirected outside its sibling directory: {}",
                path.display()
            ),
        });
    }
    Ok(canonical)
}

enum NativeHostProcessKind {
    Child {
        child: OwnedChild,
        ownership: NativeHostChildOwnership,
    },
    Empty,
}

/// Owns an optional host child for the native shell.
///
/// Isolated debug ownership terminates the child on drop. Production ownership
/// detaches only so the durable host survives client close.
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
                    NativeHostProcessKind::Child { ownership, .. } => match ownership {
                        NativeHostChildOwnership::TerminateWithClient => "child-terminate",
                        NativeHostChildOwnership::DetachOnClientClose => "child-detach",
                    },
                    NativeHostProcessKind::Empty => "empty",
                },
            )
            .finish()
    }
}

impl Drop for NativeHostProcess {
    fn drop(&mut self) {
        self.dispose(NativeShutdownDeadline::from_now(NATIVE_SHUTDOWN_BUDGET));
    }
}

impl NativeHostProcess {
    fn owned_child(child: OwnedChild, ownership: NativeHostChildOwnership) -> Self {
        Self {
            kind: NativeHostProcessKind::Child { child, ownership },
        }
    }

    fn dispose(&mut self, deadline: NativeShutdownDeadline) {
        let kind = std::mem::replace(&mut self.kind, NativeHostProcessKind::Empty);
        match kind {
            NativeHostProcessKind::Child {
                child,
                ownership: NativeHostChildOwnership::TerminateWithClient,
            } => terminate_child_with_deadline(child, deadline),
            NativeHostProcessKind::Child {
                mut child,
                ownership: NativeHostChildOwnership::DetachOnClientClose,
            } => {
                // Detach only: never kill a production durable host on client drop.
                let _ = child.child.try_wait();
            }
            NativeHostProcessKind::Empty => {}
        }
    }

    /// Dispose after `ConfirmHostQuit` has been accepted. Debug hosts must be
    /// given time to run their durable cleanup and intentional-exit path; they
    /// are not killed merely because the client window is closing.
    fn dispose_after_full_quit(&mut self, deadline: NativeShutdownDeadline) {
        let kind = std::mem::replace(&mut self.kind, NativeHostProcessKind::Empty);
        match kind {
            NativeHostProcessKind::Child {
                child,
                ownership: NativeHostChildOwnership::TerminateWithClient,
            } => wait_for_child_exit_with_deadline(child, deadline),
            NativeHostProcessKind::Child {
                mut child,
                ownership: NativeHostChildOwnership::DetachOnClientClose,
            } => {
                let _ = child.child.try_wait();
            }
            NativeHostProcessKind::Empty => {}
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
                message: "native host startup deadline expired before attach".to_string(),
            });
        }
        if !profile.is_production() {
            ensure_isolated_host_config_base(profile)?;
        }
        if deadline.expired() {
            return Err(NativeShellError::HostConnect {
                message: "native host startup deadline expired before attach".to_string(),
            });
        }

        // Attach-first: reuse a live host when the pipe is already present.
        // Missing pipe (Unavailable) falls through to a single spawn.
        // Pipe Busy gets bounded attach retries and must not be treated as absence.
        // Timeout means a present-but-slow host: retry attach, never spawn.
        loop {
            if deadline.expired() {
                return Err(NativeShellError::HostConnect {
                    message: "native host startup deadline expired before attach".to_string(),
                });
            }
            match try_attach_existing_host(profile, deadline) {
                Ok(runtime) => {
                    return Ok(NativeHostRuntimeAttachment::Client(runtime));
                }
                Err(IpcError::Unavailable) => break,
                Err(IpcError::Busy) => {
                    let remaining = deadline.remaining();
                    if remaining.is_zero() {
                        return Err(NativeShellError::HostConnect {
                            message: "native host startup deadline expired while pipe was busy"
                                .to_string(),
                        });
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(25)));
                    continue;
                }
                Err(IpcError::Timeout) => {
                    let remaining = deadline.remaining();
                    if remaining.is_zero() {
                        return Err(NativeShellError::HostConnect {
                            message: "native host attach timed out waiting for a present host"
                                .to_string(),
                        });
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(25)));
                    continue;
                }
                Err(error) => {
                    return Err(NativeShellError::HostConnect {
                        message: error.to_string(),
                    });
                }
            }
        }

        if deadline.expired() {
            return Err(NativeShellError::HostConnect {
                message: "native host startup deadline expired before launch".to_string(),
            });
        }

        let spec = if profile.is_production() {
            NativeHostLaunchSpec::for_production()?
        } else {
            NativeHostLaunchSpec::for_isolated_profile(profile, std::process::id())?
        };
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
        sanitize_spawned_host_environment(&mut command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command
            .spawn()
            .map_err(|error| NativeShellError::HostConnect {
                message: format!("devmanager-host launch failed: {error}"),
            })?;
        let process = NativeHostProcess::owned_child(
            OwnedChild {
                child,
                _permit: permit,
            },
            profile.child_ownership(),
        );
        // Concurrent clients may lose the HostLock race; bounded attach retries
        // converge on the lock winner without inventing a second shutdown path.
        let runtime =
            NativeHostClientRuntime::connect_blocking_with_process(profile, process, deadline)?;
        Ok(NativeHostRuntimeAttachment::Client(runtime))
    }
}

/// Strip parent DevManager identity overrides so the sibling host resolves only
/// from its CLI/profile contract (same removals as library/phase-gate child rules).
fn sanitize_spawned_host_environment(command: &mut Command) {
    for key in [
        "DEVMANAGER_PROFILE",
        "DEVMANAGER_INSTANCE_LABEL",
        "DEVMANAGER_RUNTIME_KIND",
        "DEVMANAGER_CONFIG_DIR",
        "DEVMANAGER_APP_IDENTITY",
    ] {
        command.env_remove(key);
    }
}

fn try_attach_existing_host(
    profile: &IsolatedDevProfile,
    deadline: NativeShutdownDeadline,
) -> Result<NativeHostClientRuntime, IpcError> {
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|_| IpcError::Unavailable)?,
    );
    let client = runtime.block_on(tokio::time::timeout(
        deadline.remaining(),
        HostClient::connect(profile.host_client_config()),
    ));
    let client = match client {
        Ok(Ok(client)) => client,
        Ok(Err(error)) => {
            if let Ok(runtime) = Arc::try_unwrap(runtime) {
                runtime.shutdown_timeout(Duration::from_millis(1));
            }
            return Err(error);
        }
        Err(_) => {
            if let Ok(runtime) = Arc::try_unwrap(runtime) {
                runtime.shutdown_timeout(Duration::from_millis(1));
            }
            // Present-but-slow connect/bootstrap must remain Timeout, never
            // Unavailable (Unavailable alone authorizes first-launch spawn).
            return Err(IpcError::Timeout);
        }
    };
    match NativeHostClientRuntime::new_with_runtime(profile, client, runtime.clone()) {
        Ok(mut runtime_owner) => {
            let bootstrap = runtime.block_on(tokio::time::timeout(
                deadline.remaining(),
                runtime_owner.bootstrap_projection(),
            ));
            match bootstrap {
                Ok(Ok(_)) => Ok(runtime_owner),
                Ok(Err(error)) => Err(IpcError::Security(error.to_string())),
                Err(_) => Err(IpcError::Timeout),
            }
        }
        Err(error) => {
            if let Ok(runtime) = Arc::try_unwrap(runtime) {
                runtime.shutdown_timeout(Duration::from_millis(1));
            }
            Err(IpcError::Security(error.to_string()))
        }
    }
}

/// Authorize ConfirmHostQuit from current inspect facts.
///
/// Host may report `confirmable=false` while worktrees are [`HostQuitWorktreeInspection::NotInspected`].
/// An explicit full-quit action may authorize that uninspected confirmation path, but
/// agent/resource blockers always fail closed.
pub(crate) fn authorize_full_host_quit(
    inspection: &HostQuitInspection,
    allow_uninspected_worktrees: bool,
) -> Result<(), String> {
    if !inspection.agents.is_empty() {
        return Err(format!(
            "host quit blocked by {} open agent session(s)",
            inspection.agents.len()
        ));
    }
    if !inspection.resources.is_empty() {
        return Err(format!(
            "host quit blocked by {} active resource(s)",
            inspection.resources.len()
        ));
    }
    if inspection.confirmable {
        return Ok(());
    }
    match inspection.worktrees {
        HostQuitWorktreeInspection::NotInspected => {
            if allow_uninspected_worktrees {
                Ok(())
            } else {
                Err(
                    "host quit requires allow_uninspected_worktrees while worktrees are NotInspected"
                        .to_string(),
                )
            }
        }
    }
}

fn ensure_isolated_host_config_base(profile: &IsolatedDevProfile) -> Result<(), NativeShellError> {
    let config_base = profile.host_config_base();
    let workspace_root = std::fs::canonicalize(profile.workspace_root()).map_err(|error| {
        NativeShellError::HostConnect {
            message: format!(
                "isolated native host workspace cannot be pinned {}: {error}",
                profile.workspace_root().display()
            ),
        }
    })?;
    let parent = config_base
        .parent()
        .ok_or_else(|| NativeShellError::HostConnect {
            message: format!(
                "isolated native host config base has no parent: {}",
                config_base.display()
            ),
        })?;
    let expected_parent = workspace_root.join(".devmanager-next");
    if parent != expected_parent {
        return Err(NativeShellError::HostConnect {
            message: format!(
                "isolated native host config base is not the generated workspace child: {}",
                config_base.display()
            ),
        });
    }
    ensure_isolated_directory(parent, Some(&workspace_root), "native host profile parent")?;
    ensure_isolated_directory(config_base, Some(parent), "native host config base")?;
    let canonical_base =
        std::fs::canonicalize(config_base).map_err(|error| NativeShellError::HostConnect {
            message: format!(
                "isolated native host config base cannot be resolved {}: {error}",
                config_base.display()
            ),
        })?;
    let canonical_parent =
        std::fs::canonicalize(parent).map_err(|error| NativeShellError::HostConnect {
            message: format!("isolated native host parent cannot be resolved: {error}"),
        })?;
    if canonical_parent.parent() != Some(workspace_root.as_path())
        || canonical_base.parent() != Some(canonical_parent.as_path())
        || !canonical_base.starts_with(&workspace_root)
    {
        return Err(NativeShellError::HostConnect {
            message: format!(
                "isolated native host config base escaped workspace: {}",
                canonical_base.display()
            ),
        });
    }
    Ok(())
}

fn ensure_isolated_directory(
    path: &Path,
    expected_parent: Option<&Path>,
    label: &str,
) -> Result<(), NativeShellError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || path_is_reparse_or_symlink(&metadata) {
                return Err(NativeShellError::HostConnect {
                    message: format!("{label} must be a real directory: {}", path.display()),
                });
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or_else(|| NativeShellError::HostConnect {
                message: format!("{label} has no parent: {}", path.display()),
            })?;
            if let Some(expected_parent) = expected_parent {
                let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
                    NativeShellError::HostConnect {
                        message: format!("{label} parent cannot be pinned: {error}"),
                    }
                })?;
                if canonical_parent != expected_parent {
                    return Err(NativeShellError::HostConnect {
                        message: format!("{label} parent escaped workspace: {}", parent.display()),
                    });
                }
            }
            std::fs::create_dir(path).map_err(|error| NativeShellError::HostConnect {
                message: format!("{label} could not be created {}: {error}", path.display()),
            })?;
            let metadata =
                std::fs::symlink_metadata(path).map_err(|error| NativeShellError::HostConnect {
                    message: format!("{label} could not be rechecked {}: {error}", path.display()),
                })?;
            if !metadata.is_dir() || path_is_reparse_or_symlink(&metadata) {
                return Err(NativeShellError::HostConnect {
                    message: format!("{label} changed into a link: {}", path.display()),
                });
            }
        }
        Err(error) => {
            return Err(NativeShellError::HostConnect {
                message: format!("{label} cannot be inspected {}: {error}", path.display()),
            });
        }
    }
    Ok(())
}

fn path_is_reparse_or_symlink(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
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
    let named_profile = workspace_profile_name(&workspace_root);
    Ok(IsolatedDevProfile {
        root: workspace_root.join(NATIVE_PROFILE_DIR),
        workspace_root,
        named_profile,
        mode: NativeShellMode::IsolatedDebug,
    })
}

/// Resolve the fail-closed production app root for release native-shell launch.
///
/// Never consults `DEVMANAGER_PROFILE`. The pipe/lock profile is the stable
/// [`PRODUCTION_HOST_PROFILE`] name while storage uses [`AppProfile::Production`].
pub fn production_shell_profile() -> Result<IsolatedDevProfile, NativeShellError> {
    if let Some(value) = env::var_os("DEVMANAGER_PROFILE") {
        return Err(NativeShellError::ProfileOverride {
            value: value.to_string_lossy().into_owned(),
        });
    }
    let config_dir = dirs::config_dir().ok_or_else(|| NativeShellError::HostConnect {
        message: "unable to resolve config directory for production profile".to_string(),
    })?;
    let config_dir = config_dir
        .canonicalize()
        .map_err(|error| NativeShellError::HostConnect {
            message: format!("unable to canonicalize config directory: {error}"),
        })?;
    let resolved = resolve_app_paths(&config_dir, AppProfile::Production, BuildKind::Release)
        .map_err(|error| NativeShellError::HostConnect {
            message: error.to_string(),
        })?;
    let expected_root = config_dir.join("com.userfirst.devmanager");
    if resolved.root != expected_root {
        return Err(NativeShellError::HostConnect {
            message: format!(
                "production profile root mismatch: {} != {}",
                resolved.root.display(),
                expected_root.display()
            ),
        });
    }
    let root = if resolved.root.exists() {
        let canonical =
            resolved
                .root
                .canonicalize()
                .map_err(|error| NativeShellError::HostConnect {
                    message: format!(
                        "unable to canonicalize production root {}: {error}",
                        resolved.root.display()
                    ),
                })?;
        let expected = if expected_root.exists() {
            expected_root
                .canonicalize()
                .map_err(|error| NativeShellError::HostConnect {
                    message: format!("unable to canonicalize expected production root: {error}"),
                })?
        } else {
            expected_root
        };
        if canonical != expected {
            return Err(NativeShellError::HostConnect {
                message: format!(
                    "production root redirected away from exact app path: {}",
                    canonical.display()
                ),
            });
        }
        let metadata = std::fs::symlink_metadata(&canonical).map_err(|error| {
            NativeShellError::HostConnect {
                message: format!("unable to inspect production root metadata: {error}"),
            }
        })?;
        if path_is_reparse_or_symlink(&metadata) {
            return Err(NativeShellError::HostConnect {
                message: format!(
                    "production root must not be a reparse point: {}",
                    canonical.display()
                ),
            });
        }
        canonical
    } else {
        expected_root
    };
    Ok(IsolatedDevProfile {
        workspace_root: root.clone(),
        root,
        named_profile: PRODUCTION_HOST_PROFILE.to_string(),
        mode: NativeShellMode::Production,
    })
}

fn workspace_profile_name(workspace_root: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(NATIVE_PROFILE_NAME_PREFIX.as_bytes());
    digest.update([0]);
    digest.update(workspace_root.to_string_lossy().as_bytes());
    let digest = digest.finalize();
    let mut suffix = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(suffix, "{byte:02x}");
    }
    format!("{NATIVE_PROFILE_NAME_PREFIX}-{suffix}")
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

#[derive(Clone, Debug, Eq, PartialEq)]
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

impl NativeActionRecord {
    fn rebind_transport_epochs(&mut self, epochs: NativeHostRuntimeEpochs, navigation_epoch: u64) {
        self.connection_epoch = epochs.connection_epoch;
        self.navigation_epoch = navigation_epoch;
        self.resource_generation = epochs.resource_generation;
        self.runtime_generation = epochs.runtime_generation;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeHostCommand {
    Envelope(crate::domain::command::CommandEnvelope),
    /// UI-thread browser surface command. It is intentionally not serialized
    /// through the generic host worker: the WebView host owns this exact
    /// lease and must apply it on its GPUI/COM thread.
    Browser(BrowserNativeHostCommand),
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
    ServiceControl {
        action_id: &'static str,
        arguments: crate::client::action::ServiceControlArguments,
        command_id: CommandId,
        issued_at_ms: i64,
    },
    ProviderInput {
        action_id: &'static str,
        arguments: crate::client::action::ProviderInputArguments,
        expected_task_revision: u64,
        command_id: CommandId,
        issued_at_ms: i64,
    },
    /// Canonical `task.show` query via [`crate::client::action::task_show_query`].
    TaskShowQuery {
        request_id: RequestId,
        task_id: TaskId,
    },
    /// Canonical host paged snapshot query for `task.list`.
    TaskListQuery {
        request_id: RequestId,
    },
    /// Host/catalog status observation through the live [`HostClient`] hello seam.
    HostStatusQuery {
        request_id: RequestId,
    },
    /// Shared action catalog observation through the live host grant seam.
    HostActionsQuery {
        request_id: RequestId,
    },
    TaskCockpitQuery {
        request_id: RequestId,
        task_id: TaskId,
        query: TaskCockpitQuery,
    },
    PromptLibraryQuery {
        request_id: RequestId,
        query: PromptLibraryQuery,
    },
    Updater {
        request_id: RequestId,
        action: UpdaterAction,
    },
    /// Explicitly surfaced typed hold. Visible query actions must never map here.
    Hold {
        action_id: &'static str,
        reason: &'static str,
    },
}

fn native_command_id(command: &NativeHostCommand) -> Option<CommandId> {
    match command {
        NativeHostCommand::Envelope(envelope) => Some(envelope.command_id),
        NativeHostCommand::Browser(_) => None,
        NativeHostCommand::TaskCreate { command_id, .. }
        | NativeHostCommand::TaskRename { command_id, .. } => Some(*command_id),
        NativeHostCommand::ServiceControl { command_id, .. } => Some(*command_id),
        NativeHostCommand::ProviderInput { command_id, .. } => Some(*command_id),
        NativeHostCommand::TaskShowQuery { .. }
        | NativeHostCommand::TaskListQuery { .. }
        | NativeHostCommand::HostStatusQuery { .. }
        | NativeHostCommand::HostActionsQuery { .. }
        | NativeHostCommand::TaskCockpitQuery { .. }
        | NativeHostCommand::PromptLibraryQuery { .. }
        | NativeHostCommand::Updater { .. }
        | NativeHostCommand::Hold { .. } => None,
    }
}

fn native_request_id(command: &NativeHostCommand) -> Option<RequestId> {
    match command {
        NativeHostCommand::TaskShowQuery { request_id, .. }
        | NativeHostCommand::TaskListQuery { request_id }
        | NativeHostCommand::HostStatusQuery { request_id }
        | NativeHostCommand::HostActionsQuery { request_id }
        | NativeHostCommand::TaskCockpitQuery { request_id, .. }
        | NativeHostCommand::PromptLibraryQuery { request_id, .. }
        | NativeHostCommand::Updater { request_id, .. } => Some(*request_id),
        _ => None,
    }
}

fn same_native_action_identity(left: &NativeActionRecord, right: &NativeActionRecord) -> bool {
    match (
        native_command_id(&left.command),
        native_command_id(&right.command),
    ) {
        (Some(left_id), Some(right_id)) => left_id == right_id,
        _ => match (
            native_request_id(&left.command),
            native_request_id(&right.command),
        ) {
            (Some(left_id), Some(right_id)) => left_id == right_id,
            _ => {
                left.id == right.id
                    && left.action_epoch == right.action_epoch
                    && left.request_generation == right.request_generation
            }
        },
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativeKeyboardState {
    pub palette_open: bool,
    pub task_switcher_open: bool,
    pub command_palette_open: bool,
    pub selected_dock: Option<DockTool>,
    pub terminal_open: bool,
    pub task_details_open: bool,
}

/// Result of handing one already-validated UI action to the host lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeHostActionResult {
    Queued,
    Disconnected,
    QueueFull,
    /// The captured action no longer matches the current focus/model fence.
    /// It remains owned by the shell for an explicit reconciliation decision.
    Stale,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeHostActionFailure {
    QueueFull {
        action_id: &'static str,
    },
    Disconnected {
        action_id: &'static str,
    },
    Stale {
        action_id: &'static str,
        command_id: Option<CommandId>,
    },
    ExecutionFailed {
        action_id: &'static str,
        command_id: Option<CommandId>,
        message: String,
    },
    ExecutionUncertain {
        action_id: &'static str,
        command_id: Option<CommandId>,
        message: String,
    },
}

impl NativeHostActionFailure {
    fn action_id(&self) -> &'static str {
        match self {
            Self::QueueFull { action_id }
            | Self::Disconnected { action_id }
            | Self::Stale { action_id, .. }
            | Self::ExecutionFailed { action_id, .. }
            | Self::ExecutionUncertain { action_id, .. } => action_id,
        }
    }

    fn command_id(&self) -> Option<CommandId> {
        match self {
            Self::ExecutionFailed { command_id, .. }
            | Self::ExecutionUncertain { command_id, .. }
            | Self::Stale { command_id, .. } => *command_id,
            Self::QueueFull { .. } | Self::Disconnected { .. } => None,
        }
    }

    fn retry_message(&self) -> String {
        match self {
            Self::QueueFull { .. } => "action retained; retry available".to_string(),
            Self::Disconnected { .. } => "action retained until host reconnects".to_string(),
            Self::Stale { .. } => {
                "action retained; current focus or model changed; reconcile explicitly".to_string()
            }
            Self::ExecutionFailed { message, .. } => {
                format!("action failed; retry available: {message}")
            }
            Self::ExecutionUncertain { message, .. } => {
                format!("outcome uncertain; retry with same command: {message}")
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeHostActionOutcome {
    Accepted {
        action: NativeActionRecord,
        receipt: crate::domain::command::CommandReceipt,
    },
    Queried {
        action: NativeActionRecord,
        detail: String,
        body: NativeHostQueryBody,
    },
    Failed {
        action: NativeActionRecord,
        error: String,
    },
    Uncertain {
        action: NativeActionRecord,
        error: String,
    },
}

impl NativeHostActionOutcome {
    fn action(&self) -> &NativeActionRecord {
        match self {
            Self::Accepted { action, .. }
            | Self::Queried { action, .. }
            | Self::Failed { action, .. }
            | Self::Uncertain { action, .. } => action,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeHostQueryBody {
    Text,
    TaskCockpit(crate::domain::TaskCockpitResult),
    PromptLibrary(PromptProjectionReply),
    Updater(UpdaterSnapshot),
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

    fn merged_monotonic(self, incoming: Self) -> Self {
        Self {
            connection_epoch: self.connection_epoch.max(incoming.connection_epoch),
            resource_generation: self.resource_generation.max(incoming.resource_generation),
            runtime_generation: self.runtime_generation.max(incoming.runtime_generation),
        }
    }

    fn is_at_least(self, other: Self) -> bool {
        self.connection_epoch >= other.connection_epoch
            && self.resource_generation >= other.resource_generation
            && self.runtime_generation >= other.runtime_generation
    }

    fn is_strictly_older_than(self, other: Self) -> bool {
        self != other && other.is_at_least(self)
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
    pub action_outcome: Option<NativeHostActionOutcome>,
}

enum NativeHostWorkerCommand {
    Execute(NativeActionRecord),
    Shutdown,
}

/// Try to hand the oldest pending action to the worker without losing it when
/// the bounded channel is saturated or has already disconnected. The action is
/// removed only after `try_send` accepts ownership; both error variants return
/// it and put it back at the exact front of the queue.
fn dispatch_pending_action(
    pending: &mut VecDeque<NativeActionRecord>,
    command_tx: &SyncSender<NativeHostWorkerCommand>,
    channel_depth: Option<&AtomicUsize>,
) -> NativeHostActionResult {
    let Some(action) = pending.pop_front() else {
        return NativeHostActionResult::Queued;
    };
    if let Some(channel_depth) = channel_depth {
        // Reserve the channel slot before try_send so a worker that receives
        // immediately cannot decrement an as-yet-unincremented counter.
        channel_depth.fetch_add(1, Ordering::AcqRel);
    }
    match command_tx.try_send(NativeHostWorkerCommand::Execute(action)) {
        Ok(()) => NativeHostActionResult::Queued,
        Err(TrySendError::Full(NativeHostWorkerCommand::Execute(action))) => {
            if let Some(channel_depth) = channel_depth {
                channel_depth.fetch_sub(1, Ordering::AcqRel);
            }
            pending.push_front(action);
            NativeHostActionResult::QueueFull
        }
        Err(TrySendError::Disconnected(NativeHostWorkerCommand::Execute(action))) => {
            if let Some(channel_depth) = channel_depth {
                channel_depth.fetch_sub(1, Ordering::AcqRel);
            }
            pending.push_front(action);
            NativeHostActionResult::Disconnected
        }
        Err(TrySendError::Full(NativeHostWorkerCommand::Shutdown))
        | Err(TrySendError::Disconnected(NativeHostWorkerCommand::Shutdown)) => {
            unreachable!("shutdown is never dispatched from the action queue")
        }
    }
}

fn handoff_pending_actions_after_shutdown(
    pending: &mut VecDeque<NativeActionRecord>,
    _command_tx: &SyncSender<NativeHostWorkerCommand>,
    _deadline: NativeShutdownDeadline,
    permit: ReaperPermit,
) {
    let message = bounded_host_error("native host shutdown before command execution completed");
    let outcomes = pending
        .drain(..)
        .map(|action| NativeHostActionOutcome::Uncertain {
            action,
            error: message.clone(),
        })
        .collect();
    retain_action_batch(outcomes, permit);
}

fn shutdown_uncertain_outcome(outcome: NativeHostActionOutcome) -> NativeHostActionOutcome {
    let action = match outcome {
        NativeHostActionOutcome::Accepted { action, .. }
        | NativeHostActionOutcome::Queried { action, .. }
        | NativeHostActionOutcome::Failed { action, .. }
        | NativeHostActionOutcome::Uncertain { action, .. } => action,
    };
    NativeHostActionOutcome::Uncertain {
        action,
        error: bounded_host_error("native shell shutdown before action reconciliation"),
    }
}

fn retain_uncertain_action_batch(outcomes: VecDeque<NativeHostActionOutcome>) {
    let outcomes = outcomes
        .into_iter()
        .map(shutdown_uncertain_outcome)
        .collect::<VecDeque<_>>();
    if outcomes.is_empty() {
        return;
    }
    if let Some(permit) = acquire_reaper_permit(ReaperKind::ActionBatch) {
        retain_action_batch(outcomes, permit);
    } else {
        for outcome in outcomes {
            let _ = retain_emergency_action_outcome(outcome);
        }
    }
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
            action_outcome: None,
        }
    }

    pub fn model(kind: NativeHostProjectionKind, model: Arc<ClientModel>) -> Self {
        Self {
            kind,
            client_model: Some(model),
            error: None,
            epochs: None,
            action_outcome: None,
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
#[derive(Clone, Debug, Eq, PartialEq)]
struct NativeHostRuntimeBinding {
    workspace_root: PathBuf,
    profile_root: PathBuf,
    named_profile: String,
}

impl NativeHostRuntimeBinding {
    fn for_profile(profile: &IsolatedDevProfile) -> Self {
        Self {
            workspace_root: profile.workspace_root().to_path_buf(),
            profile_root: profile.root().to_path_buf(),
            named_profile: profile.named_profile().to_string(),
        }
    }

    fn matches_profile(&self, profile: &IsolatedDevProfile) -> bool {
        self.workspace_root == profile.workspace_root()
            && self.profile_root == profile.root()
            && self.named_profile == profile.named_profile()
    }
}

pub(crate) struct NativeHostClientRuntime {
    binding: NativeHostRuntimeBinding,
    endpoint: String,
    /// Shared updater bound to live Host Hello (no second host FSM).
    updater: UpdaterService,
    /// `None` only while a lifecycle path temporarily owns the client so locks
    /// are never held across host awaits.
    client: Arc<Mutex<Option<HostClient>>>,
    subscription: Arc<Mutex<ClientSubscription>>,
    client_model: Arc<Mutex<Option<Arc<ClientModel>>>>,
    bootstrapped: Arc<AtomicBool>,
    pending: VecDeque<NativeActionRecord>,
    ready_projections: Arc<Mutex<VecDeque<NativeHostProjection>>>,
    ready_stream_frames: Arc<Mutex<VecDeque<StreamFrame>>>,
    command_tx: SyncSender<NativeHostWorkerCommand>,
    channel_depth: Arc<AtomicUsize>,
    cancellation: Arc<AtomicBool>,
    worker: Option<OwnedWorker>,
    action_reaper_permit: Option<ReaperPermit>,
    epochs: Arc<Mutex<NativeHostRuntimeEpochs>>,
    runtime_guard: Option<Arc<tokio::runtime::Runtime>>,
    host_process: Option<NativeHostProcess>,
    deferred_action_outcome: Arc<Mutex<Option<NativeHostActionOutcome>>>,
    worker_overflow: Arc<Mutex<VecDeque<NativeHostActionOutcome>>>,
    /// Ordinary close/detach never arms host quit; full-quit uses inspect/confirm.
    lifecycle: NativeClientLifecycle,
}

/// Explicit client-to-host lifecycle intent owned by the shell runtime.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum NativeClientLifecycle {
    #[default]
    Connected,
    /// Host-acknowledged ExplicitDetach completed; production host survives.
    Detached,
    /// confirm_host_quit was accepted through HostShutdown authority.
    FullQuitConfirmed,
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
    fn drain_projection_messages(&mut self, max: usize) -> Vec<NativeHostProjection>;
    fn take_deferred_action_outcome(&mut self) -> Option<NativeHostActionOutcome>;
    fn pending_front(&self) -> Option<&NativeActionRecord>;
    fn take_pending_front(&mut self) -> Option<NativeActionRecord>;
    fn pending_count(&self) -> usize;
    fn action_lane_count(&self) -> usize;
    fn dispatch_next_pending(&mut self) -> NativeHostActionResult;
    fn rebind_pending(&mut self, epochs: NativeHostRuntimeEpochs, navigation_epoch: u64);
    fn begin_shutdown(&mut self);
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
                    .ok()
                    .and_then(|guard| guard.as_ref().map(|client| client.is_connected()))
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
    pub(crate) async fn connect(profile: &IsolatedDevProfile) -> Result<Self, NativeShellError> {
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
        let runtime_guard = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|error| NativeShellError::HostConnect {
                    message: format!("runtime bootstrap failed: {error}"),
                })?,
        );
        let mut runtime = Self::new_with_runtime(profile, client, runtime_guard)?;
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
    pub(crate) fn connect_blocking(profile: &IsolatedDevProfile) -> Result<Self, NativeShellError> {
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
        let mut runtime_owner = match Self::new_with_runtime(profile, client, runtime.clone()) {
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
                process.dispose(deadline);
                return Err(NativeShellError::HostConnect {
                    message: format!("runtime bootstrap failed: {error}"),
                });
            }
        };
        let client = match runtime.block_on(connect_with_startup_retry(profile, deadline)) {
            Ok(client) => client,
            Err(error) => {
                let mut process = process;
                process.dispose(deadline);
                return Err(NativeShellError::HostConnect {
                    message: error.to_string(),
                });
            }
        };
        let mut runtime_owner =
            match Self::new_with_runtime_and_process(profile, client, runtime.clone(), process) {
                Ok(runtime_owner) => runtime_owner,
                Err((error, mut process)) => {
                    process.dispose(deadline);
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
                process.dispose(deadline);
            }
            return Err(error);
        }
        Ok(runtime_owner)
    }

    fn new_with_runtime(
        profile: &IsolatedDevProfile,
        client: HostClient,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Result<Self, NativeShellError> {
        Self::new_with_runtime_guard(profile, client, Some(runtime))
    }

    fn new_with_runtime_guard(
        profile: &IsolatedDevProfile,
        client: HostClient,
        runtime_guard: Option<Arc<tokio::runtime::Runtime>>,
    ) -> Result<Self, NativeShellError> {
        let install_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        let updater = UpdaterService::new();
        if let Some(install_dir) = install_dir.as_ref() {
            updater
                .observe_production_host_hello(
                    client.server_build(),
                    client.protocol_major(),
                    client.protocol_minor(),
                    install_dir,
                )
                .map_err(|error| NativeShellError::HostConnect { message: error })?;
        } else {
            updater.bind_live_host_hello(
                client.server_build(),
                client.protocol_major(),
                client.protocol_minor(),
            );
        }
        // Start the updater only after the live host identity is bound. The
        // service owns its polling thread, so this never performs network or
        // install work on the native UI thread.
        updater.start_background_checks();
        let endpoint = client.endpoint().to_string();
        let client = Arc::new(Mutex::new(Some(client)));
        let subscription = Arc::new(Mutex::new(ClientSubscription::new()));
        let client_model = Arc::new(Mutex::new(None));
        let bootstrapped = Arc::new(AtomicBool::new(false));
        let cancellation = Arc::new(AtomicBool::new(false));
        let (command_tx, command_rx) = std::sync::mpsc::sync_channel(MAX_ACTION_LANE_RECORDS);
        let channel_depth = Arc::new(AtomicUsize::new(0));
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
        let stream_frames_for_worker = Arc::new(Mutex::new(VecDeque::new()));
        let stream_frames_for_worker_thread = Arc::clone(&stream_frames_for_worker);
        let updater_for_worker = updater.clone();
        let deferred_action_outcome = Arc::new(Mutex::new(None));
        let deferred_action_outcome_for_worker = Arc::clone(&deferred_action_outcome);
        let worker_overflow = Arc::new(Mutex::new(VecDeque::new()));
        let worker_overflow_for_worker = Arc::clone(&worker_overflow);
        let channel_depth_for_worker = Arc::clone(&channel_depth);
        let action_reaper_permit =
            acquire_reaper_permit(ReaperKind::ActionBatch).ok_or_else(|| {
                NativeShellError::HostConnect {
                    message: "native host action reaper capacity exhausted".to_string(),
                }
            })?;
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
                        channel_depth_for_worker,
                        projections_for_worker_thread,
                        stream_frames_for_worker_thread,
                        updater_for_worker,
                        deferred_action_outcome_for_worker,
                        worker_overflow_for_worker,
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
            binding: NativeHostRuntimeBinding::for_profile(profile),
            endpoint,
            updater,
            client,
            subscription,
            client_model,
            bootstrapped,
            pending: VecDeque::new(),
            ready_projections: projections_for_worker,
            ready_stream_frames: stream_frames_for_worker,
            command_tx,
            channel_depth,
            cancellation,
            worker,
            action_reaper_permit: Some(action_reaper_permit),
            epochs,
            runtime_guard,
            host_process: None,
            deferred_action_outcome,
            worker_overflow,
            lifecycle: NativeClientLifecycle::Connected,
        })
    }

    fn new_with_runtime_and_process(
        profile: &IsolatedDevProfile,
        client: HostClient,
        runtime: Arc<tokio::runtime::Runtime>,
        process: NativeHostProcess,
    ) -> Result<Self, (NativeShellError, NativeHostProcess)> {
        let mut runtime_owner = match Self::new_with_runtime_guard(profile, client, Some(runtime)) {
            Ok(runtime_owner) => runtime_owner,
            Err(error) => return Err((error, process)),
        };
        runtime_owner.host_process = Some(process);
        Ok(runtime_owner)
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.client
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|client| client.is_connected()))
            .unwrap_or(false)
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) fn updater(&self) -> &UpdaterService {
        &self.updater
    }

    pub(crate) fn take_ready_stream_frames(&mut self, max: usize) -> Vec<StreamFrame> {
        let limit = max.min(MAX_HOST_PROJECTIONS);
        self.ready_stream_frames
            .lock()
            .map(|mut frames| {
                let count = limit.min(frames.len());
                frames.drain(..count).collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn subscription_id(&self) -> Option<SubscriptionId> {
        self.subscription
            .lock()
            .ok()
            .and_then(|subscription| subscription.subscription_id())
    }

    fn validate_attachment(&self, profile: &IsolatedDevProfile) -> Result<(), NativeShellError> {
        if !self.binding.matches_profile(profile) {
            return Err(NativeShellError::HostConnect {
                message: "native host runtime profile binding does not match shell profile"
                    .to_string(),
            });
        }
        if !self.bootstrapped.load(Ordering::Acquire) {
            return Err(NativeShellError::HostConnect {
                message: "native host runtime has no bootstrapped client projection".to_string(),
            });
        }
        if !self.is_connected() {
            return Err(NativeShellError::HostConnect {
                message: "native host runtime is disconnected".to_string(),
            });
        }
        Ok(())
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn action_lane_count(&self) -> usize {
        let queued_outcomes = self
            .ready_projections
            .lock()
            .map(|projections| {
                projections
                    .iter()
                    .filter(|projection| projection.action_outcome.is_some())
                    .count()
            })
            .unwrap_or_default();
        let deferred = self
            .deferred_action_outcome
            .lock()
            .map(|outcome| usize::from(outcome.is_some()))
            .unwrap_or_default();
        let worker_overflow = self
            .worker_overflow
            .lock()
            .map(|outcomes| outcomes.len())
            .unwrap_or_default();
        action_lane_total(
            self.channel_depth.load(Ordering::Acquire),
            self.pending
                .len()
                .saturating_add(queued_outcomes)
                .saturating_add(deferred)
                .saturating_add(worker_overflow),
            0,
        )
    }

    fn begin_shutdown(&mut self) {
        self.cancellation.store(true, Ordering::Release);
        if self.worker.is_some() {
            let _ = self.command_tx.try_send(NativeHostWorkerCommand::Shutdown);
        }
    }

    /// Host-acknowledged detach. Ordinary window close uses this path and never
    /// calls [`HostClient::confirm_host_quit`].
    ///
    /// Cancels/joins the subscription worker first, then takes client ownership
    /// so host awaits never run under `client`/`subscription` locks (worker uses
    /// client→subscription order; this path must not invert or nest locks).
    pub(crate) fn acknowledge_client_detach(
        &mut self,
        deadline: NativeShutdownDeadline,
    ) -> Result<Uuid, NativeShellError> {
        if matches!(
            self.lifecycle,
            NativeClientLifecycle::Detached | NativeClientLifecycle::FullQuitConfirmed
        ) {
            return Ok(Uuid::nil());
        }
        self.begin_shutdown();
        if let Some(worker) = self.worker.take() {
            join_worker_with_deadline(worker, deadline);
        }
        let Some(runtime) = self.runtime_guard.as_ref() else {
            return Err(NativeShellError::HostConnect {
                message: "native host runtime unavailable for client detach".to_string(),
            });
        };
        let remaining = deadline.remaining();
        if remaining.is_zero() {
            return Err(NativeShellError::HostConnect {
                message: "native host detach deadline expired".to_string(),
            });
        }

        // Take ownership without holding locks across host awaits.
        let mut client = self
            .client
            .lock()
            .map_err(|_| NativeShellError::HostConnect {
                message: "native host client lock poisoned during detach".to_string(),
            })?
            .take()
            .ok_or_else(|| NativeShellError::HostConnect {
                message: "native host client unavailable during detach".to_string(),
            })?;
        let mut subscription = {
            let mut guard =
                self.subscription
                    .lock()
                    .map_err(|_| NativeShellError::HostConnect {
                        message: "native host subscription lock poisoned during detach".to_string(),
                    })?;
            std::mem::replace(&mut *guard, ClientSubscription::new())
        };

        let result = runtime.block_on(async {
            tokio::time::timeout(remaining, async {
                let _ = subscription.release(&mut client).await;
                client.detach().await
            })
            .await
        });

        // Restore owned handles (detached client stays disconnected).
        if let Ok(mut guard) = self.subscription.lock() {
            *guard = subscription;
        }
        if let Ok(mut guard) = self.client.lock() {
            *guard = Some(client);
        }

        match result {
            Ok(Ok(connection_id)) => {
                self.lifecycle = NativeClientLifecycle::Detached;
                Ok(connection_id)
            }
            Ok(Err(error)) => Err(NativeShellError::HostConnect {
                message: format!("acknowledged client detach failed: {error}"),
            }),
            Err(_) => Err(NativeShellError::HostConnect {
                message: "acknowledged client detach deadline expired".to_string(),
            }),
        }
    }

    /// Explicit full quit through existing HostShutdown inspect/confirm authority.
    ///
    /// Uses inspect facts for fail-closed blocker checks. When worktrees are
    /// `NotInspected` and `confirmable` is false, only an explicitly authorized
    /// `allow_uninspected_worktrees` confirmation may proceed.
    pub(crate) fn confirm_full_host_quit(
        &mut self,
        allow_uninspected_worktrees: bool,
        deadline: NativeShutdownDeadline,
    ) -> Result<crate::domain::command::CommandReceipt, NativeShellError> {
        if matches!(self.lifecycle, NativeClientLifecycle::FullQuitConfirmed) {
            return Err(NativeShellError::HostConnect {
                message: "full host quit already confirmed on this client".to_string(),
            });
        }
        self.begin_shutdown();
        if let Some(worker) = self.worker.take() {
            join_worker_with_deadline(worker, deadline);
        }
        let Some(runtime) = self.runtime_guard.as_ref() else {
            return Err(NativeShellError::HostConnect {
                message: "native host runtime unavailable for full quit".to_string(),
            });
        };
        let remaining = deadline.remaining();
        if remaining.is_zero() {
            return Err(NativeShellError::HostConnect {
                message: "native host full-quit deadline expired".to_string(),
            });
        }

        let mut client = self
            .client
            .lock()
            .map_err(|_| NativeShellError::HostConnect {
                message: "native host client lock poisoned during full quit".to_string(),
            })?
            .take()
            .ok_or_else(|| NativeShellError::HostConnect {
                message: "native host client unavailable during full quit".to_string(),
            })?;

        let result = runtime.block_on(async {
            tokio::time::timeout(remaining, async {
                let inspection = match client.inspect_host_quit().await? {
                    Ok(inspection) => inspection,
                    Err(error) => {
                        return Err(IpcError::Security(format!(
                            "inspect_host_quit rejected: {error:?}"
                        )));
                    }
                };
                authorize_full_host_quit(&inspection, allow_uninspected_worktrees)
                    .map_err(|message| IpcError::Security(message))?;
                client
                    .confirm_host_quit(
                        CommandId::new(),
                        inspection.inspection_id,
                        allow_uninspected_worktrees,
                    )
                    .await
            })
            .await
        });

        if let Ok(mut guard) = self.client.lock() {
            *guard = Some(client);
        }

        match result {
            Ok(Ok(receipt)) => {
                self.lifecycle = NativeClientLifecycle::FullQuitConfirmed;
                Ok(receipt)
            }
            Ok(Err(error)) => Err(NativeShellError::HostConnect {
                message: format!("full host quit failed: {error}"),
            }),
            Err(_) => Err(NativeShellError::HostConnect {
                message: "full host quit deadline expired".to_string(),
            }),
        }
    }

    /// Queue one action without performing transport work on the UI thread.
    pub(crate) fn enqueue(&mut self, action: NativeActionRecord) -> NativeHostActionResult {
        if !self.is_connected() {
            return NativeHostActionResult::Disconnected;
        }
        if self.pending.len() >= MAX_PENDING_HOST_ACTIONS
            || self.action_lane_count() >= MAX_ACTION_LANE_RECORDS
        {
            return NativeHostActionResult::QueueFull;
        }
        self.pending.push_back(action);
        let result = dispatch_pending_action(
            &mut self.pending,
            &self.command_tx,
            Some(&self.channel_depth),
        );
        if !matches!(result, NativeHostActionResult::Queued) {
            // Admission failure belongs to the shell retry lane; do not leave
            // the same identity in both queues.
            let _ = self.pending.pop_back();
        }
        result
    }

    fn pending_front(&self) -> Option<&NativeActionRecord> {
        self.pending.front()
    }

    fn take_pending_front(&mut self) -> Option<NativeActionRecord> {
        self.pending.pop_front()
    }

    fn dispatch_next_pending(&mut self) -> NativeHostActionResult {
        dispatch_pending_action(
            &mut self.pending,
            &self.command_tx,
            Some(&self.channel_depth),
        )
    }

    fn rebind_pending(&mut self, epochs: NativeHostRuntimeEpochs, navigation_epoch: u64) {
        for action in &mut self.pending {
            action.rebind_transport_epochs(epochs, navigation_epoch);
        }
    }

    pub fn take_ready_projection_messages(&mut self, max: usize) -> Vec<NativeHostProjection> {
        let limit = max.min(MAX_HOST_PROJECTIONS);
        let mut projections: Vec<NativeHostProjection> = self
            .ready_projections
            .lock()
            .map(|mut projections| {
                let count = limit.min(projections.len());
                projections.drain(..count).collect()
            })
            .unwrap_or_default();
        while projections.len() < limit {
            let Some(outcome) = self
                .worker_overflow
                .lock()
                .ok()
                .and_then(|mut overflow| overflow.pop_front())
            else {
                break;
            };
            projections.push(NativeHostProjection {
                kind: NativeHostProjectionKind::Error,
                client_model: None,
                error: Some(
                    "native host worker action outcome retained under queue pressure".to_string(),
                ),
                epochs: Some(current_runtime_epochs(&self.epochs)),
                action_outcome: Some(outcome),
            });
        }
        projections
    }

    /// Perform the initial bounded five-section snapshot/replay handoff owned
    /// by the one client subscription. GPUI receives an immutable model; it
    /// never observes raw snapshot pages or a task-only transport projection.
    pub async fn bootstrap_projection(
        &mut self,
    ) -> Result<Vec<NativeHostProjectionKind>, NativeShellError> {
        // Take ownership before the await. Holding either mutex across host
        // I/O would deadlock lifecycle paths and previously passed the
        // `MutexGuard<Option<HostClient>>` itself to `synchronize`.
        let mut client_owned = self
            .client
            .lock()
            .map_err(|_| NativeShellError::HostConnect {
                message: "native host client lock poisoned".to_string(),
            })?
            .take()
            .ok_or_else(|| NativeShellError::HostConnect {
                message: "native host client unavailable during bootstrap".to_string(),
            })?;
        let mut subscription_owned = {
            let mut guard =
                self.subscription
                    .lock()
                    .map_err(|_| NativeShellError::HostConnect {
                        message: "native host subscription lock poisoned".to_string(),
                    })?;
            std::mem::replace(&mut *guard, ClientSubscription::new())
        };
        let synchronized = subscription_owned.synchronize(&mut client_owned).await;
        let model = synchronized
            .map_err(|error| NativeShellError::HostConnect {
                message: error.to_string(),
            })
            .and_then(|()| {
                subscription_owned
                    .model()
                    .cloned()
                    .map(Arc::new)
                    .ok_or_else(|| NativeShellError::HostConnect {
                        message: "native host subscription produced no client model".to_string(),
                    })
            });
        let restore_subscription = self.subscription.lock().map(|mut guard| {
            *guard = subscription_owned;
        });
        let restore_client = self.client.lock().map(|mut guard| {
            *guard = Some(client_owned);
        });
        restore_subscription.map_err(|_| NativeShellError::HostConnect {
            message: "native host subscription lock poisoned during bootstrap restore".to_string(),
        })?;
        restore_client.map_err(|_| NativeShellError::HostConnect {
            message: "native host client lock poisoned during bootstrap restore".to_string(),
        })?;
        let model = model?;
        if let Ok(mut current) = self.client_model.lock() {
            *current = Some(Arc::clone(&model));
        }
        let epochs = current_runtime_epochs(&self.epochs);
        publish_projection(
            &self.ready_projections,
            NativeHostProjection::client_model(Arc::clone(&model)).at_epochs(epochs),
        );
        for _ in 0..MAX_HOST_PROJECTIONS.saturating_sub(1) {
            publish_projection(
                &self.ready_projections,
                NativeHostProjection::kind(NativeHostProjectionKind::Replay).at_epochs(epochs),
            );
        }
        self.bootstrapped.store(true, Ordering::Release);
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
        let mut guard = self.client.lock().map_err(|_| IpcError::Unavailable)?;
        let client = guard.as_mut().ok_or(IpcError::Unavailable)?;
        client.execute_command(envelope).await
    }

    fn take_ready_action_outcomes_for_shutdown(&mut self) -> VecDeque<NativeHostActionOutcome> {
        let mut outcomes = VecDeque::new();
        if let Ok(mut projections) = self.ready_projections.lock() {
            for projection in projections.drain(..) {
                if let Some(outcome) = projection.action_outcome {
                    outcomes.push_back(outcome);
                }
            }
        }
        if let Ok(mut overflow) = self.worker_overflow.lock() {
            outcomes.extend(overflow.drain(..));
        }
        outcomes
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

    fn drain_projection_messages(&mut self, max: usize) -> Vec<NativeHostProjection> {
        self.take_ready_projection_messages(max)
    }

    fn take_deferred_action_outcome(&mut self) -> Option<NativeHostActionOutcome> {
        self.deferred_action_outcome
            .lock()
            .ok()
            .and_then(|mut outcome| outcome.take())
    }

    fn pending_front(&self) -> Option<&NativeActionRecord> {
        NativeHostClientRuntime::pending_front(self)
    }

    fn take_pending_front(&mut self) -> Option<NativeActionRecord> {
        NativeHostClientRuntime::take_pending_front(self)
    }

    fn pending_count(&self) -> usize {
        NativeHostClientRuntime::pending_count(self)
    }

    fn action_lane_count(&self) -> usize {
        NativeHostClientRuntime::action_lane_count(self)
    }

    fn dispatch_next_pending(&mut self) -> NativeHostActionResult {
        NativeHostClientRuntime::dispatch_next_pending(self)
    }

    fn rebind_pending(&mut self, epochs: NativeHostRuntimeEpochs, navigation_epoch: u64) {
        NativeHostClientRuntime::rebind_pending(self, epochs, navigation_epoch)
    }

    fn begin_shutdown(&mut self) {
        NativeHostClientRuntime::begin_shutdown(self)
    }
}

impl native_host_runtime_sealed::Sealed for NativeHostClientRuntime {}

impl Drop for NativeHostClientRuntime {
    fn drop(&mut self) {
        let deadline = NativeShutdownDeadline::from_now(NATIVE_SHUTDOWN_BUDGET);
        // Invalidate the worker before transferring any pending identity to
        // shutdown retention. No Drop path is allowed to enqueue a fresh
        // Execute after cancellation begins.
        self.begin_shutdown();
        reap_retained_workers();
        reap_retained_children();
        if !self.pending.is_empty() {
            let permit = self
                .action_reaper_permit
                .take()
                .expect("runtime action reaper permit is reserved at construction");
            handoff_pending_actions_after_shutdown(
                &mut self.pending,
                &self.command_tx,
                deadline,
                permit,
            );
        }
        if let Some(worker) = self.worker.take() {
            join_worker_with_deadline(worker, deadline);
        }
        let mut shutdown_outcomes = self.take_ready_action_outcomes_for_shutdown();
        if let Some(outcome) = self.take_deferred_action_outcome() {
            shutdown_outcomes.push_back(outcome);
        }
        if !shutdown_outcomes.is_empty() {
            retain_uncertain_action_batch(shutdown_outcomes);
        }
        match self.lifecycle {
            NativeClientLifecycle::Connected => {
                // Ordinary window close: acknowledged detach only. Never
                // inspect/confirm host quit from Drop.
                let _ = self.acknowledge_client_detach(deadline);
            }
            NativeClientLifecycle::Detached | NativeClientLifecycle::FullQuitConfirmed => {
                if let Some(runtime) = self.runtime_guard.as_ref() {
                    // client → subscription order; no host awaits under nested locks.
                    let mut client = match self.client.lock() {
                        Ok(mut guard) => guard.take(),
                        Err(_) => None,
                    };
                    let mut subscription = match self.subscription.lock() {
                        Ok(mut guard) => {
                            Some(std::mem::replace(&mut *guard, ClientSubscription::new()))
                        }
                        Err(_) => None,
                    };
                    if let (Some(ref mut client), Some(ref mut subscription)) =
                        (client.as_mut(), subscription.as_mut())
                    {
                        let remaining = deadline.remaining();
                        if !remaining.is_zero() {
                            let _ = runtime.block_on(async {
                                tokio::time::timeout(remaining, subscription.release(client)).await
                            });
                        }
                    }
                    if let (Ok(mut guard), Some(subscription)) =
                        (self.subscription.lock(), subscription)
                    {
                        *guard = subscription;
                    }
                    if let (Ok(mut guard), Some(client)) = (self.client.lock(), client) {
                        *guard = Some(client);
                    }
                }
            }
        }
        if let Some(process) = self.host_process.as_mut() {
            // Production DetachOnClientClose never kills the durable host here.
            // A debug full quit is different from an ordinary client close:
            // wait for the host's accepted cleanup/intentional-exit path before
            // releasing its TerminateWithClient child handle.
            if matches!(self.lifecycle, NativeClientLifecycle::FullQuitConfirmed) {
                process.dispose_after_full_quit(deadline);
            } else {
                process.dispose(deadline);
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
    client: Arc<Mutex<Option<HostClient>>>,
    subscription: Arc<Mutex<ClientSubscription>>,
    client_model: Arc<Mutex<Option<Arc<ClientModel>>>>,
    bootstrapped: Arc<AtomicBool>,
    runtime: Option<Arc<tokio::runtime::Runtime>>,
    cancellation: Arc<AtomicBool>,
    epochs: Arc<Mutex<NativeHostRuntimeEpochs>>,
    command_rx: Receiver<NativeHostWorkerCommand>,
    channel_depth: Arc<AtomicUsize>,
    projections: Arc<Mutex<VecDeque<NativeHostProjection>>>,
    stream_frames: Arc<Mutex<VecDeque<StreamFrame>>>,
    updater: UpdaterService,
    deferred_action_outcome: Arc<Mutex<Option<NativeHostActionOutcome>>>,
    worker_overflow: Arc<Mutex<VecDeque<NativeHostActionOutcome>>>,
) {
    let Some(runtime) = runtime else {
        return;
    };
    loop {
        if !bootstrapped.load(Ordering::Acquire) {
            if cancellation.load(Ordering::Acquire) {
                drain_cancelled_worker_commands(
                    &command_rx,
                    &channel_depth,
                    &projections,
                    &deferred_action_outcome,
                    &worker_overflow,
                    &epochs,
                    &cancellation,
                );
                break;
            }
            std::thread::sleep(CONTROLLER_TICK_INTERVAL);
            continue;
        }
        match command_rx.recv_timeout(CONTROLLER_TICK_INTERVAL) {
            Ok(NativeHostWorkerCommand::Shutdown) => {
                drain_cancelled_worker_commands(
                    &command_rx,
                    &channel_depth,
                    &projections,
                    &deferred_action_outcome,
                    &worker_overflow,
                    &epochs,
                    &cancellation,
                );
                break;
            }
            Ok(NativeHostWorkerCommand::Execute(action)) => {
                channel_depth.fetch_sub(1, Ordering::AcqRel);
                if cancellation.load(Ordering::Acquire) {
                    publish_cancelled_action_outcome(
                        action,
                        &projections,
                        &deferred_action_outcome,
                        &worker_overflow,
                        &epochs,
                        &cancellation,
                    );
                    drain_cancelled_worker_commands(
                        &command_rx,
                        &channel_depth,
                        &projections,
                        &deferred_action_outcome,
                        &worker_overflow,
                        &epochs,
                        &cancellation,
                    );
                    break;
                }
                let projection = match action.command.clone() {
                    NativeHostCommand::Hold { action_id, reason } => {
                        let message = bounded_host_error(format!("{action_id}: HOLD: {reason}"));
                        NativeHostProjection {
                            kind: NativeHostProjectionKind::Error,
                            client_model: None,
                            error: Some(message.clone()),
                            epochs: None,
                            action_outcome: Some(NativeHostActionOutcome::Failed {
                                action,
                                error: message,
                            }),
                        }
                    }
                    command => {
                        let result = client.lock().ok().and_then(|mut guard| {
                            let client = guard.as_mut()?;
                            Some(runtime.block_on(execute_native_command_cancellable(
                                client,
                                command,
                                &updater,
                                &cancellation,
                            )))
                        });
                        match result {
                            Some(Ok(NativeHostExecutionResult::Command(receipt))) => {
                                let kind = match &receipt {
                                    crate::domain::command::CommandReceipt::Rejected { .. } => {
                                        NativeHostProjectionKind::Error
                                    }
                                    crate::domain::command::CommandReceipt::Accepted { .. } => {
                                        NativeHostProjectionKind::Live
                                    }
                                };
                                NativeHostProjection {
                                    kind,
                                    client_model: None,
                                    error: None,
                                    epochs: None,
                                    action_outcome: Some(NativeHostActionOutcome::Accepted {
                                        action,
                                        receipt,
                                    }),
                                }
                            }
                            Some(Ok(NativeHostExecutionResult::Query { detail, body })) => {
                                NativeHostProjection {
                                    kind: NativeHostProjectionKind::Live,
                                    client_model: None,
                                    error: None,
                                    epochs: None,
                                    action_outcome: Some(NativeHostActionOutcome::Queried {
                                        action,
                                        detail,
                                        body,
                                    }),
                                }
                            }
                            Some(Ok(NativeHostExecutionResult::QueryFailed(error))) => {
                                NativeHostProjection {
                                    kind: NativeHostProjectionKind::Error,
                                    client_model: None,
                                    error: Some(error.clone()),
                                    epochs: None,
                                    action_outcome: Some(NativeHostActionOutcome::Failed {
                                        action,
                                        error,
                                    }),
                                }
                            }
                            Some(Err(error)) => {
                                let message = bounded_host_error(error.to_string());
                                NativeHostProjection {
                                    kind: NativeHostProjectionKind::Error,
                                    client_model: None,
                                    error: Some(message.clone()),
                                    epochs: None,
                                    action_outcome: Some(NativeHostActionOutcome::Uncertain {
                                        action,
                                        error: message,
                                    }),
                                }
                            }
                            None => {
                                let message = "native host client lock poisoned".to_string();
                                NativeHostProjection {
                                    kind: NativeHostProjectionKind::Error,
                                    client_model: None,
                                    error: Some(message.clone()),
                                    epochs: None,
                                    action_outcome: Some(NativeHostActionOutcome::Uncertain {
                                        action,
                                        error: message,
                                    }),
                                }
                            }
                        }
                    }
                }
                .at_epochs(current_runtime_epochs(&epochs));
                publish_worker_projection(
                    &projections,
                    &deferred_action_outcome,
                    &worker_overflow,
                    projection,
                    &cancellation,
                );
            }
            Err(RecvTimeoutError::Timeout) => {
                if cancellation.load(Ordering::Acquire) {
                    drain_cancelled_worker_commands(
                        &command_rx,
                        &channel_depth,
                        &projections,
                        &deferred_action_outcome,
                        &worker_overflow,
                        &epochs,
                        &cancellation,
                    );
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                drain_cancelled_worker_commands(
                    &command_rx,
                    &channel_depth,
                    &projections,
                    &deferred_action_outcome,
                    &worker_overflow,
                    &epochs,
                    &cancellation,
                );
                break;
            }
        }
        if cancellation.load(Ordering::Acquire) {
            drain_cancelled_worker_commands(
                &command_rx,
                &channel_depth,
                &projections,
                &deferred_action_outcome,
                &worker_overflow,
                &epochs,
                &cancellation,
            );
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
            &stream_frames,
        );
    }
}

fn publish_cancelled_action_outcome(
    action: NativeActionRecord,
    projections: &Arc<Mutex<VecDeque<NativeHostProjection>>>,
    deferred_action_outcome: &Arc<Mutex<Option<NativeHostActionOutcome>>>,
    worker_overflow: &Arc<Mutex<VecDeque<NativeHostActionOutcome>>>,
    epochs: &Arc<Mutex<NativeHostRuntimeEpochs>>,
    cancellation: &Arc<AtomicBool>,
) {
    let message = "native host shutdown before command execution completed".to_string();
    publish_worker_projection(
        projections,
        deferred_action_outcome,
        worker_overflow,
        NativeHostProjection {
            kind: NativeHostProjectionKind::Error,
            client_model: None,
            error: Some(message.clone()),
            epochs: Some(current_runtime_epochs(epochs)),
            action_outcome: Some(NativeHostActionOutcome::Uncertain {
                action,
                error: message,
            }),
        },
        cancellation,
    );
}

fn drain_cancelled_worker_commands(
    command_rx: &Receiver<NativeHostWorkerCommand>,
    channel_depth: &Arc<AtomicUsize>,
    projections: &Arc<Mutex<VecDeque<NativeHostProjection>>>,
    deferred_action_outcome: &Arc<Mutex<Option<NativeHostActionOutcome>>>,
    worker_overflow: &Arc<Mutex<VecDeque<NativeHostActionOutcome>>>,
    epochs: &Arc<Mutex<NativeHostRuntimeEpochs>>,
    cancellation: &Arc<AtomicBool>,
) {
    while let Ok(command) = command_rx.try_recv() {
        if let NativeHostWorkerCommand::Execute(action) = command {
            channel_depth.fetch_sub(1, Ordering::AcqRel);
            publish_cancelled_action_outcome(
                action,
                projections,
                deferred_action_outcome,
                worker_overflow,
                epochs,
                cancellation,
            );
        }
    }
}

fn publish_worker_projection(
    projections: &Arc<Mutex<VecDeque<NativeHostProjection>>>,
    deferred_action_outcome: &Arc<Mutex<Option<NativeHostActionOutcome>>>,
    worker_overflow: &Arc<Mutex<VecDeque<NativeHostActionOutcome>>>,
    projection: NativeHostProjection,
    cancellation: &Arc<AtomicBool>,
) {
    let Some(outcome) = projection.action_outcome.clone() else {
        let _ = publish_projection(projections, projection);
        return;
    };
    if cancellation.load(Ordering::Acquire) {
        if !retain_emergency_action_outcome(outcome) {
            cancellation.store(true, Ordering::Release);
        }
        return;
    }
    if publish_projection(projections, projection) {
        return;
    }
    if let Ok(mut deferred) = deferred_action_outcome.lock() {
        if deferred.is_none() {
            *deferred = Some(outcome.clone());
            return;
        }
    }
    if let Ok(mut overflow) = worker_overflow.lock() {
        if overflow.len() < MAX_ACTION_LANE_RECORDS {
            overflow.push_back(outcome.clone());
            return;
        }
    }

    // The normal projection, deferred slot, and second overflow lane are all
    // bounded by the aggregate action admission cap. This emergency ledger is
    // bounded as well and prevents a worker from spinning forever when a
    // shutdown races a saturated controller queue.
    if !retain_emergency_action_outcome(outcome) {
        cancellation.store(true, Ordering::Release);
    }
}

fn pump_subscription_once(
    client: &Arc<Mutex<Option<HostClient>>>,
    subscription: &Arc<Mutex<ClientSubscription>>,
    client_model: &Arc<Mutex<Option<Arc<ClientModel>>>>,
    runtime: &tokio::runtime::Runtime,
    cancellation: &Arc<AtomicBool>,
    epochs: &Arc<Mutex<NativeHostRuntimeEpochs>>,
    projections: &Arc<Mutex<VecDeque<NativeHostProjection>>>,
    stream_frames: &Arc<Mutex<VecDeque<StreamFrame>>>,
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
                    action_outcome: None,
                }
                .at_epochs(current_runtime_epochs(epochs)),
            );
            return;
        };
        let Some(client_ref) = client_guard.as_ref() else {
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
                    action_outcome: None,
                }
                .at_epochs(current_runtime_epochs(epochs)),
            );
            return;
        };
        runtime.block_on(tokio::time::timeout(
            Duration::from_millis(2),
            subscription_guard.recv_and_apply(client_ref),
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
                    .ok()
                    .and_then(|guard| guard.as_ref().map(|client| client.is_connected()))
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
                                error: Some(bounded_host_error(resync_error)),
                                epochs: None,
                                action_outcome: None,
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
                .ok()
                .and_then(|guard| guard.as_ref().map(|client| client.is_connected()))
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
                Err(error) => {
                    let _ = publish_projection(
                        projections,
                        NativeHostProjection {
                            kind: NativeHostProjectionKind::Error,
                            client_model: None,
                            error: Some(bounded_host_error(error)),
                            epochs: None,
                            action_outcome: None,
                        }
                        .at_epochs(current_runtime_epochs(epochs)),
                    );
                }
            }
        }
        SubscriptionUpdate::Stream(frame) => {
            if let Ok(mut frames) = stream_frames.lock() {
                if frames.len() < MAX_HOST_PROJECTIONS {
                    frames.push_back(frame);
                }
            }
            publish_projection(
                projections,
                NativeHostProjection::kind(NativeHostProjectionKind::Live)
                    .at_epochs(current_runtime_epochs(epochs)),
            );
        }
    }
}

fn resynchronize_subscription(
    client: &Arc<Mutex<Option<HostClient>>>,
    subscription: &Arc<Mutex<ClientSubscription>>,
    runtime: &tokio::runtime::Runtime,
    cancellation: &Arc<AtomicBool>,
    deadline: NativeShutdownDeadline,
) -> Result<(Arc<ClientModel>, bool), String> {
    // Take ownership so host awaits do not run under mutex guards.
    let mut client_owned = client
        .lock()
        .map_err(|_| "native host client lock poisoned".to_string())?
        .take()
        .ok_or_else(|| "native host client unavailable during resync".to_string())?;
    let mut subscription_owned = {
        let mut guard = subscription
            .lock()
            .map_err(|_| "native host subscription lock poisoned".to_string())?;
        std::mem::replace(&mut *guard, ClientSubscription::new())
    };
    let was_connected = client_owned.is_connected();
    let result = runtime.block_on(async {
        tokio::time::timeout(deadline.remaining(), async {
            tokio::select! {
                result = async {
                    if client_owned.is_connected() {
                        subscription_owned
                            .release(&mut client_owned)
                            .await
                            .map_err(|error| error.to_string())?;
                    }
                    if !client_owned.is_connected() {
                        client_owned
                            .reconnect()
                            .await
                            .map_err(|error| error.to_string())?;
                    }
                    subscription_owned = ClientSubscription::new();
                    subscription_owned
                        .synchronize(&mut client_owned)
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
    let model = match result {
        Ok(()) => subscription_owned
            .model()
            .cloned()
            .map(Arc::new)
            .ok_or_else(|| "native host resync produced no client model".to_string()),
        Err(error) => Err(error),
    };
    if let Ok(mut guard) = subscription.lock() {
        *guard = subscription_owned;
    }
    if let Ok(mut guard) = client.lock() {
        *guard = Some(client_owned);
    }
    let model = model?;
    Ok((model, !was_connected))
}

fn publish_projection(
    projections: &Arc<Mutex<VecDeque<NativeHostProjection>>>,
    projection: NativeHostProjection,
) -> bool {
    let Ok(mut queue) = projections.lock() else {
        return false;
    };
    let action_outcome_count = queue
        .iter()
        .filter(|queued| queued.action_outcome.is_some())
        .count();
    let is_action_outcome = projection.action_outcome.is_some();
    if is_action_outcome {
        if action_outcome_count >= MAX_ACTION_OUTCOME_PROJECTIONS {
            // At most the bounded worker/action lane can be in flight.
            // Never evict an earlier exact action outcome if that
            // invariant is violated; the queue remains bounded and the
            // controller's saturation state is the visible signal.
            return false;
        }
        if queue.len() >= MAX_HOST_PROJECTION_MESSAGES {
            // Preserve action outcomes over stale subscription noise.
            let Some(index) = queue
                .iter()
                .position(|queued| queued.action_outcome.is_none())
            else {
                return false;
            };
            let _ = queue.remove(index);
        }
        queue.push_back(projection);
        return true;
    }

    let normal_count = queue.len().saturating_sub(action_outcome_count);
    if normal_count < MAX_HOST_PROJECTIONS && queue.len() < MAX_HOST_PROJECTION_MESSAGES {
        queue.push_back(projection);
        return true;
    }
    false
}

async fn execute_native_command(
    client: &mut HostClient,
    command: NativeHostCommand,
    updater: &UpdaterService,
) -> Result<NativeHostExecutionResult, IpcError> {
    match command {
        NativeHostCommand::Envelope(envelope) => client
            .execute_command(envelope)
            .await
            .map(NativeHostExecutionResult::Command),
        // BrowserNativeHostCommand is applied by the owning NativeShell host
        // on the UI thread. It must never be downgraded into a generic IPC
        // command or a fresh browser instance.
        NativeHostCommand::Browser(_) => Err(IpcError::Unavailable),
        NativeHostCommand::TaskCreate {
            arguments,
            command_id,
            issued_at_ms,
        } => {
            let envelope = crate::client::action::task_create_command(
                command_id,
                client.client_id(),
                issued_at_ms,
                arguments,
            )
            .map_err(|_| IpcError::Unavailable)?;
            client
                .execute_command(envelope)
                .await
                .map(NativeHostExecutionResult::Command)
        }
        NativeHostCommand::TaskRename {
            arguments,
            expected_task_revision,
            command_id,
            issued_at_ms,
        } => {
            let envelope = crate::client::action::task_rename_command(
                command_id,
                client.client_id(),
                issued_at_ms,
                expected_task_revision,
                arguments,
            )
            .map_err(|_| IpcError::Unavailable)?;
            client
                .execute_command(envelope)
                .await
                .map(NativeHostExecutionResult::Command)
        }
        NativeHostCommand::ServiceControl {
            action_id,
            arguments,
            command_id,
            issued_at_ms,
        } => {
            let envelope = crate::client::action::service_control_command(
                command_id,
                client.client_id(),
                issued_at_ms,
                action_id,
                arguments,
            )
            .map_err(|_| IpcError::Unavailable)?;
            client
                .execute_command(envelope)
                .await
                .map(NativeHostExecutionResult::Command)
        }
        NativeHostCommand::ProviderInput {
            action_id,
            arguments,
            expected_task_revision,
            command_id,
            issued_at_ms,
        } => {
            let envelope = crate::client::action::provider_input_command(
                command_id,
                client.client_id(),
                issued_at_ms,
                expected_task_revision,
                action_id,
                arguments,
            )
            .map_err(|_| IpcError::Unavailable)?;
            client
                .execute_command(envelope)
                .await
                .map(NativeHostExecutionResult::Command)
        }
        NativeHostCommand::TaskShowQuery {
            request_id,
            task_id,
        } => {
            // Keep the action on the canonical task.show factory path even when
            // HostClient allocates its own transport request id for the round-trip.
            let _canonical =
                crate::client::action::task_show_query(request_id, client.client_id(), task_id);
            let snapshot = match client.task_snapshot(task_id).await? {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return Ok(NativeHostExecutionResult::QueryFailed(bounded_host_error(
                        format!("task.show query failed: {error:?}"),
                    )))
                }
            };
            Ok(query_text(bounded_host_error(format!(
                "task.show · {} · rev {}",
                snapshot.task.title, snapshot.task.revision
            ))))
        }
        NativeHostCommand::TaskListQuery { request_id } => {
            let _ = request_id;
            let page = match client
                .snapshot_page(SnapshotSection::Tasks, None, None)
                .await?
            {
                Ok(page) => page,
                Err(error) => {
                    return Ok(NativeHostExecutionResult::QueryFailed(bounded_host_error(
                        format!("task.list query failed: {error:?}"),
                    )))
                }
            };
            let detail = bounded_host_error(format!(
                "task.list · {} items · through {}",
                page.items.len(),
                page.through_sequence
            ));
            match client.release_snapshot(page.snapshot_id).await? {
                Ok(()) => Ok(query_text(detail)),
                Err(error) => Ok(NativeHostExecutionResult::QueryFailed(bounded_host_error(
                    format!("task.list snapshot release failed: {error:?}"),
                ))),
            }
        }
        NativeHostCommand::HostStatusQuery { request_id } => {
            let _ = request_id;
            Ok(query_text(bounded_host_error(format!(
                "host.status · boot={} · connection={} · build={} · protocol={}.{}",
                client.host_boot_id(),
                client.connection_id(),
                client.server_build(),
                client.protocol_major(),
                client.protocol_minor()
            ))))
        }
        NativeHostCommand::HostActionsQuery { request_id } => {
            let _ = request_id;
            let granted = client.granted_capabilities();
            let enabled = action::catalog()
                .iter()
                .filter(|descriptor| action::action_enabled(descriptor.id, granted))
                .count();
            Ok(query_text(bounded_host_error(format!(
                "host.actions · {} catalog · {} enabled under current grants",
                action::catalog().len(),
                enabled
            ))))
        }
        NativeHostCommand::TaskCockpitQuery {
            request_id,
            task_id,
            query,
        } => {
            let _ = request_id;
            let action_id = action::cockpit_query_action_id(&query);
            match client.query_task_cockpit(task_id, query).await? {
                Ok(result) => Ok(NativeHostExecutionResult::Query {
                    detail: bounded_host_error(format!(
                        "{} · {:?}",
                        action_id,
                        std::mem::discriminant(&result)
                    )),
                    body: NativeHostQueryBody::TaskCockpit(result),
                }),
                Err(error) => Ok(NativeHostExecutionResult::QueryFailed(bounded_host_error(
                    format!("task cockpit query failed: {error:?}"),
                ))),
            }
        }
        NativeHostCommand::PromptLibraryQuery { request_id, query } => {
            let _ = request_id;
            match client.query_prompt_library(query).await? {
                Ok(reply) => Ok(NativeHostExecutionResult::Query {
                    detail: bounded_host_error("prompt.library query"),
                    body: NativeHostQueryBody::PromptLibrary(reply),
                }),
                Err(error) => Ok(NativeHostExecutionResult::QueryFailed(bounded_host_error(
                    format!("prompt library query failed: {error:?}"),
                ))),
            }
        }
        NativeHostCommand::Updater { request_id, action } => {
            let _ = request_id;
            let action_error = match action {
                UpdaterAction::StartBackground => {
                    updater.start_background_checks();
                    None
                }
                UpdaterAction::Check => updater.check_for_updates().err(),
                UpdaterAction::Download => updater.download_update().err(),
                UpdaterAction::Install => None,
            };
            let snapshot = updater.snapshot();
            if let Some(error) = action_error {
                return Ok(NativeHostExecutionResult::QueryFailed(bounded_host_error(
                    error,
                )));
            }
            Ok(NativeHostExecutionResult::Query {
                detail: bounded_host_error(format!("{} · {:?}", action.id(), snapshot.stage)),
                body: NativeHostQueryBody::Updater(snapshot),
            })
        }
        NativeHostCommand::Hold { .. } => Err(IpcError::Unavailable),
    }
}

fn query_text(detail: String) -> NativeHostExecutionResult {
    NativeHostExecutionResult::Query {
        detail,
        body: NativeHostQueryBody::Text,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NativeHostExecutionResult {
    Command(crate::domain::command::CommandReceipt),
    Query {
        detail: String,
        body: NativeHostQueryBody,
    },
    QueryFailed(String),
}

async fn execute_native_command_cancellable(
    client: &mut HostClient,
    command: NativeHostCommand,
    updater: &UpdaterService,
    cancellation: &Arc<AtomicBool>,
) -> Result<NativeHostExecutionResult, IpcError> {
    tokio::select! {
        result = execute_native_command(client, command, updater) => result,
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

fn update_state_from_stage(stage: UpdaterStage) -> UpdateState {
    match stage {
        UpdaterStage::Disabled => UpdateState::Disabled,
        UpdaterStage::Idle => UpdateState::Idle,
        UpdaterStage::Checking => UpdateState::Checking,
        UpdaterStage::UpToDate => UpdateState::UpToDate,
        UpdaterStage::UpdateAvailable => UpdateState::Available,
        UpdaterStage::Downloading => UpdateState::Downloading,
        UpdaterStage::ReadyToInstall => UpdateState::ReadyToInstall,
        UpdaterStage::Installing => UpdateState::Installing,
        UpdaterStage::Error => UpdateState::Error,
    }
}

fn service_panel_action_label(action: ServicePanelAction) -> &'static str {
    match action {
        ServicePanelAction::Start => "Start",
        ServicePanelAction::Stop => "Stop",
        ServicePanelAction::Restart => "Restart",
        ServicePanelAction::Logs => "Logs",
        ServicePanelAction::Health => "Health",
        ServicePanelAction::OpenTerminal => "Terminal",
    }
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
    pointer_gesture: PointerGesture,
    last_handler: Option<HandlerTrace>,
    keyboard_state: NativeKeyboardState,
    pending_keyboard: Option<(FocusEpoch, u64, KeyboardAction)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PointerGesture {
    Idle,
    Down { pointer_id: u64, consumed: bool },
    Released { pointer_id: u64, consumed: bool },
}

impl NativeInteraction {
    pub fn new(selected_task: Option<TaskId>) -> Self {
        Self {
            shell: Shell::detached(selected_task),
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
            pointer_gesture: PointerGesture::Idle,
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

    fn host_runtime_epochs(&self) -> NativeHostRuntimeEpochs {
        NativeHostRuntimeEpochs {
            connection_epoch: self.connection_epoch,
            resource_generation: self.resource_generation,
            runtime_generation: self.runtime_generation,
        }
    }

    pub(crate) fn sync_host_epochs(&mut self, epochs: NativeHostRuntimeEpochs) -> bool {
        let epochs = self.host_runtime_epochs().merged_monotonic(epochs);
        let changed = self.connection_epoch != epochs.connection_epoch
            || self.resource_generation != epochs.resource_generation
            || self.runtime_generation != epochs.runtime_generation;
        if changed {
            self.connection_epoch = epochs.connection_epoch;
            self.resource_generation = epochs.resource_generation;
            self.runtime_generation = epochs.runtime_generation;
            self.shell.on_resync();
            self.pending_keyboard = None;
            self.pointer_capture = None;
            self.pointer_owner = None;
            self.pointer_gesture = PointerGesture::Idle;
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

    /// Outcome admission permits a newer client_epoch when a queued current
    /// ClientModel projection advanced the projection counter after capture.
    /// Transport, navigation, runtime, resource, focus, and task fences stay exact.
    pub fn accepts_action_outcome_record(&self, record: &NativeActionRecord) -> bool {
        if record.client_epoch > self.client_epoch {
            return false;
        }
        let mut adjusted = record.clone();
        adjusted.client_epoch = self.client_epoch;
        self.accepts_action_record(&adjusted)
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
        if self.shell.selected_task() == selected_task {
            return false;
        }
        let Some(navigation_epoch) = self.shell.navigation_epoch().checked_add(1) else {
            return false;
        };
        self.replace_shell_selection(selected_task, navigation_epoch);
        true
    }

    fn replace_shell_selection(&mut self, selected_task: Option<TaskId>, navigation_epoch: u64) {
        let epochs = crate::ui::shell::HostEpochSnapshot::try_from_host(
            self.resource_generation.max(1),
            self.connection_epoch.max(1),
            self.shell.focus_navigation_epoch().max(1),
            self.client_epoch.max(1),
            navigation_epoch.max(1),
        );
        self.shell = epochs
            .ok()
            .map(|epochs| Shell::new(selected_task, epochs))
            .unwrap_or_else(|| Shell::detached(selected_task));
        self.pointer_capture = None;
        self.pointer_owner = None;
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

    fn note_pointer_down(&mut self, pointer_id: u64, consume: bool) {
        match self.pointer_gesture {
            PointerGesture::Idle | PointerGesture::Released { .. } => {
                self.pointer_gesture = PointerGesture::Down {
                    pointer_id,
                    consumed: consume,
                };
            }
            PointerGesture::Down {
                pointer_id: owner,
                consumed,
            } => {
                self.pointer_gesture = PointerGesture::Down {
                    pointer_id: owner,
                    consumed: consumed || consume,
                };
            }
        }
    }

    fn pointer_action_blocked(&self, pointer_id: u64) -> bool {
        match self.pointer_gesture {
            PointerGesture::Down {
                pointer_id: owner,
                consumed,
            }
            | PointerGesture::Released {
                pointer_id: owner,
                consumed,
            } => consumed && owner == pointer_id,
            PointerGesture::Idle => false,
        }
    }

    /// Start a new exclusive control gesture after the previous pointer has
    /// been released. A consume that already owns this down is preserved so an
    /// overlapping task/terminal click cannot be rewritten as a toolbar click.
    pub fn begin_control_pointer(&mut self, pointer_id: u64) {
        self.note_pointer_down(pointer_id, false);
    }

    pub fn release_pointer(&mut self, pointer_id: u64) {
        if let PointerGesture::Down {
            pointer_id: owner,
            consumed,
        } = self.pointer_gesture
        {
            if owner == pointer_id {
                self.pointer_gesture = PointerGesture::Released {
                    pointer_id,
                    consumed,
                };
            }
        }
    }

    /// Selecting a task or terminal consumes the pointer. An overlapping
    /// [`InteractionStateModel`] must not activate on the same down/up.
    pub fn overlapping_control_pointer_up(
        &mut self,
        control: &mut InteractionStateModel,
        pointer_id: u64,
        down_epoch: FocusEpoch,
    ) -> bool {
        if self.pointer_action_blocked(pointer_id) {
            let _ = control.try_set_focus_epoch(self.focus_epochs.current());
            return false;
        }
        control.pointer_up(pointer_id, down_epoch)
    }

    pub fn bind_projected_model(&mut self, model: Arc<ClientModel>) {
        self.client_model = Some(Arc::clone(&model));
        self.client_epoch = model.last_applied_sequence();
        self.pointer_gesture = PointerGesture::Idle;
        let selected = self
            .selected_task()
            .filter(|task_id| model.tasks().contains_key(task_id));
        let navigation_epoch = self.shell.navigation_epoch().max(1);
        self.replace_shell_selection(selected, navigation_epoch);
    }

    pub fn task_header(&self, model: &ClientModel) -> Option<TaskHeaderModel> {
        self.shell.task_header(model)
    }

    pub fn navigation_mouse_down(
        &mut self,
        task_id: TaskId,
        task_list: &TaskList,
    ) -> NavigationHandlerOutcome {
        self.navigation_mouse_down_for(NATIVE_POINTER_ID, task_id, task_list)
    }

    pub fn navigation_mouse_down_for(
        &mut self,
        pointer_id: u64,
        task_id: TaskId,
        task_list: &TaskList,
    ) -> NavigationHandlerOutcome {
        self.note_pointer_down(pointer_id, true);
        let (focus_epoch, request_generation) = self.begin_handler(Some(task_id));
        let navigation = if task_list
            .task_ids()
            .iter()
            .any(|candidate| *candidate == task_id)
        {
            match self.shell.navigation_epoch().checked_add(1) {
                Some(navigation_epoch) => {
                    self.replace_shell_selection(Some(task_id), navigation_epoch);
                    NavigationResult::Committed {
                        task_id,
                        navigation_epoch,
                    }
                }
                None => NavigationResult::Rejected {
                    reason: crate::ui::shell::NavigationRejection::EpochExhausted,
                },
            }
        } else {
            self.pointer_capture = None;
            self.pointer_owner = None;
            NavigationResult::Rejected {
                reason: crate::ui::shell::NavigationRejection::TaskNotInInbox,
            }
        };
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
        self.note_pointer_down(pointer_id, true);
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
        self.release_pointer(pointer_id);
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
            KeyboardAction::OpenTaskDetails => {
                self.keyboard_state.task_details_open = true;
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
        if let ActivationSource::Pointer { pointer_id } = source {
            if self.pointer_action_blocked(pointer_id) {
                return None;
            }
        }
        let selected_task = self.selected_task();
        let request_task = match &request {
            ActionRequest::TaskShow { task_id } => Some(*task_id),
            ActionRequest::TaskRename(arguments) => Some(arguments.task_id),
            ActionRequest::ProviderInput(arguments) => Some(arguments.arguments.task_id),
            ActionRequest::TaskCockpit { task_id, .. } => Some(*task_id),
            ActionRequest::Browser(arguments) => Some(arguments.task_id()),
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
            ActionRequest::ProviderInput(arguments) => {
                let model = self.client_model.as_ref()?;
                let task = model.tasks().get(&arguments.arguments.task_id)?;
                if task.task.revision == 0
                    || arguments.arguments.runtime_generation == 0
                    || arguments.arguments.action_epoch == 0
                    || arguments.arguments.action_epoch != task.task.action_epoch
                    || arguments.arguments.runtime_generation != self.runtime_generation
                {
                    return None;
                }
                (Some(task.task.revision), Some(task.task.action_epoch))
            }
            _ => (None, None),
        };
        // Service control fences are captured against the public action epoch
        // before begin_handler advances it. Comparing after the increment would
        // reject every correctly fenced ActionRequest.
        if let ActionRequest::ServiceControl { arguments, .. } = &request {
            if arguments.resource_generation != self.resource_generation
                || arguments.connection_epoch != self.connection_epoch
                || arguments.action_epoch != self.action_epoch
                || self.resource_generation == 0
                || self.connection_epoch == 0
                || self.action_epoch == 0
            {
                return None;
            }
        }
        let (focus_epoch, request_generation) = self.begin_handler(self.selected_task());
        let command_id = CommandId::new();
        let request_id = RequestId::new();
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
            ActionRequest::ServiceControl { arguments, .. } => NativeHostCommand::ServiceControl {
                action_id: request.id(),
                arguments: arguments.clone(),
                command_id,
                issued_at_ms,
            },
            ActionRequest::ProviderInput(arguments) => NativeHostCommand::ProviderInput {
                action_id: arguments.action_id,
                arguments: arguments.arguments.clone(),
                expected_task_revision: expected_task_revision
                    .expect("provider input revision was validated above"),
                command_id,
                issued_at_ms,
            },
            ActionRequest::Browser(BrowserActionRequest { command }) => {
                NativeHostCommand::Browser(command.clone())
            }
            ActionRequest::HostActions => NativeHostCommand::HostActionsQuery { request_id },
            ActionRequest::HostStatus => NativeHostCommand::HostStatusQuery { request_id },
            ActionRequest::TaskList => NativeHostCommand::TaskListQuery { request_id },
            ActionRequest::TaskShow { task_id } => NativeHostCommand::TaskShowQuery {
                request_id,
                task_id: *task_id,
            },
            ActionRequest::TaskCockpit { task_id, query } => NativeHostCommand::TaskCockpitQuery {
                request_id,
                task_id: *task_id,
                query: query.clone(),
            },
            ActionRequest::PromptLibrary { query } => NativeHostCommand::PromptLibraryQuery {
                request_id,
                query: query.clone(),
            },
            ActionRequest::Updater(action) => NativeHostCommand::Updater {
                request_id,
                action: *action,
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
        self.metadata.name()
    }

    pub fn description(&self) -> &str {
        self.metadata.description()
    }

    pub fn role(&self) -> AccessibleRole {
        self.metadata.role()
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
                row.metadata.set_focused(selected_task == Some(*task_id));
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
        let prompt_library = AccessibilityNode::new(
            AccessibleRole::Region,
            "Prompt Library and Composer",
            "Personal saved prompts and the task composer use typed host/client projections.",
        )
        .gpui("native-shell-prompt-composer", false, false);
        let context_dock = AccessibilityNode::new(
            AccessibleRole::Region,
            "Task context dock",
            "Workspace, Git, files, SSH, browser, services, artifacts, review, and terminal tabs follow the selected task.",
        )
        .gpui("native-shell-context-dock", false, false);
        let root = AccessibilityNode::new(
            AccessibleRole::Region,
            "Task Cockpit",
            "Native GPUI shell using an isolated dev/test host profile.",
        )
        .gpui("native-shell-root", true, true)
        .with_children(vec![toolbar, inbox, prompt_library, context_dock, terminal]);
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
            AccessibleRole::Menu => Role::Menu,
            AccessibleRole::TextField => Role::TextInput,
            AccessibleRole::Status => Role::Status,
            AccessibleRole::Alert => Role::Alert,
            AccessibleRole::Region => Role::Region,
            AccessibleRole::Tab => Role::Tab,
            AccessibleRole::TabList => Role::TabList,
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
        if metadata.disabled() {
            node.set_disabled();
        }
        if metadata.busy() {
            node.set_busy();
        }
        if metadata.read_only() {
            node.set_read_only();
        }
        if metadata.invalid() {
            node.set_invalid(accesskit::Invalid::Grammar);
        }
        if let Some(value) = metadata.value() {
            node.set_value(value.to_string());
        }
        if let Some(error) = metadata.error() {
            node.set_description(format!("{} {}", source.description(), error));
        }
        if source.metadata().focused() {
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
    /// Owns the local `/api/connect` listener in the native process.
    remote_host_service: Option<RemoteHostService>,
    /// Keeps the IPC-backed Connect executor alive until the listener joins.
    connect_host_port: Option<Arc<HostClientConnectPort>>,
    host_state: NativeHostState,
    preferences: RuntimePreferencesSnapshot,
    header_attachment: NativeHeaderAttachment,
    client_model: Option<Arc<ClientModel>>,
    inbox: Inbox,
    task_list: TaskList,
    cockpit: TaskCockpitShell,
    /// The one host-owned WebView2 authority used by the active GPUI shell.
    /// Tests use the unsupported implementation so they never touch an
    /// installed browser profile or create a real WebView.
    browser_host: BrowserWebViewHost,
    prompt_library: PromptLibrarySession,
    services_projection: ServicesPanelProjection,
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
    pending_host_actions: VecDeque<NativeActionRecord>,
    retained_action_overflow: Option<NativeActionRecord>,
    last_action_failure: Option<NativeHostActionFailure>,
    last_action_receipt: Option<crate::domain::command::CommandReceipt>,
    last_query_detail: Option<String>,
    top_bar: Option<TopBarProjectionController>,
    composer: Option<TaskComposer>,
    composer_error: Option<String>,
    updater_snapshot: Option<UpdaterSnapshot>,
    pending_preferences: VecDeque<RuntimePreferencesSnapshot>,
    appearance_subscription: Option<Subscription>,
    bounds_subscription: Option<Subscription>,
    platform_accessibility: NativePlatformAccessibilityBridge,
}

impl Drop for NativeShell {
    fn drop(&mut self) {
        if self.cockpit.browser_projection().is_some() {
            if let Ok(command) = self.cockpit.detach_browser_native() {
                let _ = self.browser_host.apply_native_shell_command(&command);
                self.cockpit.finish_browser_detach();
            }
        }
        let _ = self.browser_host.drain_events();

        // Stop new Connect dispatch before joining the web listener. The
        // listener then observes the typed unavailable result while the
        // bounded HostClient map is dropped with this shell.
        if self.connect_host_port.take().is_some() {
            crate::connect::unbind_host_request_handle();
        }
        self.remote_host_service.take();
        // Invalidate the transport before transferring any shell-owned action
        // into shutdown retention. Dropping the shell must never turn a
        // pending identity into a fresh Execute.
        let mut outcomes = VecDeque::new();
        if let Some(runtime) = self.host_runtime.as_mut() {
            match runtime {
                NativeHostRuntimeAttachment::Injected(runtime) => {
                    runtime.begin_shutdown();
                }
                NativeHostRuntimeAttachment::Client(runtime) => {
                    runtime.begin_shutdown();
                }
            }
        }
        outcomes.extend(self.pending_host_actions.drain(..).map(|action| {
            NativeHostActionOutcome::Uncertain {
                action,
                error: bounded_host_error("native shell dropped before action reconciliation"),
            }
        }));
        if let Some(action) = self.retained_action_overflow.take() {
            outcomes.push_back(NativeHostActionOutcome::Uncertain {
                action,
                error: bounded_host_error("native shell dropped before action reconciliation"),
            });
        }
        if let Some(runtime) = self.host_runtime.as_mut() {
            // The shell owns the attachment boundary, including injected test
            // ports. Drain the bounded runtime-pending lane here instead of
            // relying on a concrete runtime destructor to perform the handoff.
            // This keeps every accepted identity durable even when an
            // attachment is replaced by a sealed port implementation.
            let pending_count = match runtime {
                NativeHostRuntimeAttachment::Injected(runtime) => {
                    runtime.pending_count().min(MAX_ACTION_LANE_RECORDS)
                }
                NativeHostRuntimeAttachment::Client(runtime) => {
                    runtime.pending_count().min(MAX_ACTION_LANE_RECORDS)
                }
            };
            for _ in 0..pending_count {
                let pending = match runtime {
                    NativeHostRuntimeAttachment::Injected(runtime) => runtime.take_pending_front(),
                    NativeHostRuntimeAttachment::Client(runtime) => runtime.take_pending_front(),
                };
                let Some(action) = pending else {
                    break;
                };
                outcomes.push_back(NativeHostActionOutcome::Uncertain {
                    action,
                    error: bounded_host_error("native shell dropped before action reconciliation"),
                });
            }
            let projections = match runtime {
                NativeHostRuntimeAttachment::Injected(runtime) => {
                    runtime.drain_projection_messages(MAX_HOST_PROJECTION_MESSAGES)
                }
                NativeHostRuntimeAttachment::Client(runtime) => {
                    runtime.drain_projection_messages(MAX_HOST_PROJECTION_MESSAGES)
                }
            };
            for projection in projections {
                if let Some(outcome) = projection.action_outcome {
                    outcomes.push_back(outcome);
                }
            }
            let deferred = match runtime {
                NativeHostRuntimeAttachment::Injected(runtime) => {
                    runtime.take_deferred_action_outcome()
                }
                NativeHostRuntimeAttachment::Client(runtime) => {
                    runtime.take_deferred_action_outcome()
                }
            };
            if let Some(outcome) = deferred {
                outcomes.push_back(outcome);
            }
        }
        retain_uncertain_action_batch(outcomes);
    }
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

    pub(crate) fn new_with_host_runtime(
        profile: IsolatedDevProfile,
        host_runtime: Option<NativeHostClientRuntime>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (host_runtime, host_state) = validated_runtime_attachment(&profile, host_runtime);
        Self::new_with_host_runtime_and_state_and_preferences(
            profile,
            host_runtime,
            host_state,
            RuntimePreferencesSnapshot::default(),
            cx,
        )
    }

    pub(crate) fn new_with_host_runtime_and_preferences(
        profile: IsolatedDevProfile,
        host_runtime: Option<NativeHostClientRuntime>,
        preferences: RuntimePreferencesSnapshot,
        cx: &mut Context<Self>,
    ) -> Self {
        let (host_runtime, host_state) = validated_runtime_attachment(&profile, host_runtime);
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
        let (connect_host_port, remote_host_service) = if start_controller {
            let mut host_config = profile.host_client_config();
            host_config.requested = CapabilitySet::from_capabilities([
                Capability::PagedSnapshots,
                Capability::EventReplay,
                Capability::OperationSettlement,
                Capability::ChunkResume,
                Capability::PromptProjection,
                Capability::ProviderInput,
                Capability::TaskCockpit,
                Capability::HostShutdown,
                Capability::ExplicitDetach,
                Capability::ServiceSupervisor,
            ]);
            let bridge = Arc::new(HostClientConnectPort::new(host_config));
            crate::connect::bind_host_executor(bridge.clone());
            let remote_config = crate::remote::load_remote_machine_state()
                .map(|state| state.host)
                .unwrap_or_default();
            (Some(bridge), Some(RemoteHostService::new(remote_config)))
        } else {
            (None, None)
        };
        let inbox = Inbox::from_error(crate::ui::task_cockpit::InboxError::ProjectionUnavailable);
        let task_list = TaskList::empty();
        let prompt_library = PromptLibrarySession::new(PromptLibraryViewport {
            scheme: ColorScheme::Dark,
            density: Density::Compact,
            scale: ScalePercent::OneHundred,
            width: LayoutWidth::Wide,
            data: DataFixtureKind::Empty,
        });
        let services_projection = ServicesPanelProjection::default();
        let header_attachment = NativeHeaderAttachment::default();
        let accessibility_tree =
            AccessibilityTree::for_task_list_with_header(&task_list, None, &header_attachment);
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
        let browser_profile_root = profile.root().to_path_buf();
        let mut shell = Self {
            host_connection: profile.host_connection(),
            profile,
            host_runtime,
            remote_host_service,
            connect_host_port,
            host_state,
            preferences,
            header_attachment,
            client_model: None,
            inbox,
            task_list,
            cockpit: TaskCockpitShell::new(DockEdge::Bottom),
            browser_host: {
                #[cfg(test)]
                {
                    BrowserWebViewHost::unavailable("native shell tests")
                }
                #[cfg(not(test))]
                {
                    BrowserWebViewHost::new(&browser_profile_root)
                }
            },
            prompt_library,
            services_projection,
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
            pending_host_actions: VecDeque::new(),
            retained_action_overflow: None,
            last_action_failure: None,
            last_action_receipt: None,
            last_query_detail: None,
            top_bar: None,
            composer: None,
            composer_error: None,
            updater_snapshot: None,
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
            &self.task_list,
            self.interaction.selected_task(),
            &self.header_attachment,
        );
        self.platform_accessibility.sync(&self.accessibility_tree);
    }

    pub fn last_keyboard_action(&self) -> Option<KeyboardAction> {
        self.last_keyboard_action
    }

    pub fn last_action_failure(&self) -> Option<&NativeHostActionFailure> {
        self.last_action_failure.as_ref()
    }

    /// The most recent command receipt remains attached to the shell so a
    /// host response can be reconciled by its exact CommandId. UI status is
    /// derived separately from this durable response identity.
    pub fn last_action_receipt(&self) -> Option<&crate::domain::command::CommandReceipt> {
        self.last_action_receipt.as_ref()
    }

    pub fn last_query_detail(&self) -> Option<&str> {
        self.last_query_detail.as_deref()
    }

    pub fn browser_host_status(&self) -> crate::browser::BrowserHostStatus {
        self.browser_host.status()
    }

    /// Apply a controller-admitted native command through the sole active
    /// WebView host. This is the production call seam used by the native
    /// action path; it never starts a second browser/window owner.
    pub fn apply_browser_native_command(
        &mut self,
        command: &BrowserNativeHostCommand,
    ) -> Result<BrowserNativeHostOutcome, BrowserError> {
        let outcome = self.browser_host.apply_native_shell_command(command)?;
        if matches!(command, BrowserNativeHostCommand::Detach { .. }) {
            self.cockpit.finish_browser_detach();
        }
        self.forward_browser_host_events();
        Ok(outcome)
    }

    /// Submit through the existing BrowserWebViewHost command authority after
    /// the native lease fence has accepted the handoff. The GPUI window is
    /// only borrowed for the owning WebView/COM thread call.
    pub fn submit_browser_native_command(
        &mut self,
        window: &Window,
        command: &BrowserNativeHostCommand,
    ) -> Result<BrowserResponse, BrowserError> {
        let outcome = self.browser_host.apply_native_shell_command(command)?;
        if outcome != BrowserNativeHostOutcome::CommandHandoff {
            return Err(BrowserError::InvalidInvocation {
                field: "browserCommand".to_string(),
            });
        }
        let workspace_key =
            command
                .workspace_key()
                .ok_or_else(|| BrowserError::InvalidInvocation {
                    field: "workspaceKey".to_string(),
                })?;
        let browser_command =
            command
                .browser_command()
                .ok_or_else(|| BrowserError::InvalidInvocation {
                    field: "browserCommand".to_string(),
                })?;
        let response =
            self.browser_host
                .handle_command(window, workspace_key, browser_command.clone())?;
        self.forward_browser_host_events();
        Ok(response)
    }

    fn forward_browser_host_events(&mut self) {
        for event in self.browser_host.drain_events() {
            let _ = self.cockpit.forward_browser_host_event(&event);
        }
    }

    fn apply_query_body(&mut self, body: NativeHostQueryBody) {
        match body {
            NativeHostQueryBody::Text => {}
            NativeHostQueryBody::TaskCockpit(result) => {
                self.cockpit.apply_cockpit_result(&result);
                if let crate::domain::TaskCockpitResult::Services(services) = &result {
                    self.services_projection = project_services_from_task_projection(services);
                }
            }
            NativeHostQueryBody::PromptLibrary(reply) => {
                if let Err(error) = apply_host_reply_to_session(&mut self.prompt_library, &reply) {
                    self.prompt_library.load = crate::ui::prompts::PromptLibraryLoadState::Error {
                        message: error.to_string(),
                    };
                }
            }
            NativeHostQueryBody::Updater(snapshot) => {
                self.updater_snapshot = Some(snapshot);
                self.sync_top_bar_from_updater();
            }
        }
    }

    fn refresh_selected_cockpit_surfaces(&mut self) {
        let Some(task_id) = self.interaction.selected_task() else {
            return;
        };
        self.cockpit
            .begin_cockpit_query(task_id, crate::client::action::ACTION_GIT_STATUS);
        for action_id in [
            crate::client::action::ACTION_WORKSPACE_STATUS,
            crate::client::action::ACTION_GIT_STATUS,
            crate::client::action::ACTION_FILES_LIST,
            crate::client::action::ACTION_SSH_STATUS,
        ] {
            if let Some(request) = action::task_cockpit_request(task_id, action_id) {
                let _ = self.dispatch_action(request);
            }
        }
        let _ = self.dispatch_action(ActionRequest::TaskCockpit {
            task_id,
            query: TaskCockpitQuery::ServiceSnapshots,
        });
    }

    fn hydrate_prompt_library(&mut self) {
        self.prompt_library.load = crate::ui::prompts::PromptLibraryLoadState::Loading;
        let _ = self.dispatch_action(ActionRequest::PromptLibrary {
            query: PromptLibraryQuery::MetadataPage {
                namespace: PromptNamespace::Personal,
                cursor: None,
                expected_revision: self.prompt_library.expected_revision,
            },
        });
        let _ = self.dispatch_action(ActionRequest::PromptLibrary {
            query: PromptLibraryQuery::ChainPage {
                chain_id: None,
                cursor: None,
                expected_revision: self.prompt_library.expected_revision,
            },
        });
    }

    fn admit_ready_stream_frames(&mut self, max: usize) {
        let Some(subscription_id) = self
            .host_runtime
            .as_ref()
            .and_then(|runtime| match runtime {
                NativeHostRuntimeAttachment::Client(runtime) => runtime.subscription_id(),
                NativeHostRuntimeAttachment::Injected(_) => None,
            })
        else {
            return;
        };
        let frames = match self.host_runtime.as_mut() {
            Some(NativeHostRuntimeAttachment::Client(runtime)) => {
                runtime.take_ready_stream_frames(max)
            }
            _ => Vec::new(),
        };
        for frame in frames {
            let _ = self
                .cockpit
                .dock_mut()
                .admit_subscription_stream(subscription_id, &frame);
        }
        self.sync_terminal_from_cockpit();
    }

    fn sync_top_bar_from_updater(&mut self) {
        let snapshot = self.updater_snapshot.clone().or_else(|| {
            self.host_runtime
                .as_ref()
                .and_then(|runtime| match runtime {
                    NativeHostRuntimeAttachment::Client(runtime) => {
                        Some(runtime.updater().snapshot())
                    }
                    NativeHostRuntimeAttachment::Injected(_) => None,
                })
        });
        let Some(snapshot) = snapshot else {
            return;
        };
        self.updater_snapshot = Some(snapshot.clone());
        let now_ms = unix_time_ms();
        let generation = self
            .interaction
            .host_runtime_epochs()
            .resource_generation
            .max(1);
        let update = update_observation_from_snapshot(
            &snapshot.current_version,
            snapshot.target_version.as_deref(),
            update_state_from_stage(snapshot.stage),
            now_ms,
            generation,
            generation,
        );
        let input = TopBarProjectionInput {
            now_ms,
            generation,
            host: None,
            connect: None,
            update: Some(update),
            quotas: one_fresh_quota_observations(&[], now_ms),
            resources: None,
        };
        match self.top_bar.as_mut() {
            Some(controller) => {
                let _ = controller.apply(input);
            }
            None => {
                if let Ok(controller) = TopBarProjectionController::new(input) {
                    self.top_bar = Some(controller);
                }
            }
        }
        self.sync_header_projection();
    }

    #[allow(dead_code)]
    fn dispatch_composer_pending(
        &mut self,
        turn_id: Option<crate::domain::TurnId>,
        question_id: Option<crate::domain::QuestionId>,
        approval_id: Option<crate::domain::ApprovalId>,
    ) -> Result<(), ComposerError> {
        let intent = self
            .composer
            .as_ref()
            .and_then(TaskComposer::pending_intent)
            .cloned()
            .ok_or(ComposerError::UnknownTurn)?;
        let request = intent.to_provider_input_request(turn_id, question_id, approval_id)?;
        self.composer_error = None;
        if self.dispatch_action(request).is_some() {
            self.composer_error = Some("composer action was not accepted by the host lane".into());
        }
        Ok(())
    }

    pub fn cockpit(&self) -> &TaskCockpitShell {
        &self.cockpit
    }

    pub fn terminal_state(&self) -> TerminalDockState {
        self.terminal.state()
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

    #[cfg(test)]
    fn pending_action_for_test(&self) -> Option<&NativeActionRecord> {
        self.pending_host_actions.front()
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

    fn action_lane_len(&self) -> usize {
        let runtime_lane = self
            .host_runtime
            .as_ref()
            .map(|runtime| match runtime {
                NativeHostRuntimeAttachment::Injected(runtime) => runtime.action_lane_count(),
                NativeHostRuntimeAttachment::Client(runtime) => runtime.action_lane_count(),
            })
            .unwrap_or(0);
        action_lane_total(
            runtime_lane,
            self.pending_host_actions.len(),
            usize::from(self.retained_action_overflow.is_some()),
        )
    }

    fn rebind_pending_host_actions(
        &mut self,
        epochs: NativeHostRuntimeEpochs,
        navigation_epoch: u64,
    ) {
        for action in &mut self.pending_host_actions {
            action.rebind_transport_epochs(epochs, navigation_epoch);
        }
        if let Some(action) = self.retained_action_overflow.as_mut() {
            action.rebind_transport_epochs(epochs, navigation_epoch);
        }
        if let Some(runtime) = self.host_runtime.as_mut() {
            match runtime {
                NativeHostRuntimeAttachment::Injected(runtime) => {
                    runtime.rebind_pending(epochs, navigation_epoch)
                }
                NativeHostRuntimeAttachment::Client(runtime) => {
                    runtime.rebind_pending(epochs, navigation_epoch)
                }
            }
        }
    }

    fn rebind_pending_client_epoch(&mut self, client_epoch: u64) {
        for action in &mut self.pending_host_actions {
            action.client_epoch = client_epoch;
        }
        if let Some(action) = self.retained_action_overflow.as_mut() {
            action.client_epoch = client_epoch;
        }
    }

    /// Reconcile retained actions only at an explicit caller boundary.
    /// Controller ticks observe outcomes and transport state but never
    /// resubmit a failed or uncertain command on their own.
    pub fn reconcile_pending_host_actions(&mut self, max: usize) {
        for _ in 0..max.min(MAX_ACTION_LANE_RECORDS) {
            let from_overflow = self.pending_host_actions.is_empty();
            let Some(action) = self
                .pending_host_actions
                .front()
                .cloned()
                .or_else(|| self.retained_action_overflow.clone())
            else {
                break;
            };
            if !self.interaction.accepts_action_record(&action) {
                self.set_transport_failure(&action, NativeHostActionResult::Stale);
                break;
            }
            match self.try_enqueue_host_action(action.clone()) {
                NativeHostActionResult::Queued => {
                    if from_overflow {
                        self.retained_action_overflow = None;
                    } else {
                        let _ = self.pending_host_actions.pop_front();
                    }
                    self.clear_recovered_action_failure();
                }
                result @ (NativeHostActionResult::Disconnected
                | NativeHostActionResult::QueueFull
                | NativeHostActionResult::Stale) => {
                    self.set_transport_failure(&action, result);
                    break;
                }
            }
        }
    }

    fn retain_pending_host_action(&mut self, action: NativeActionRecord) -> bool {
        if self
            .pending_host_actions
            .iter()
            .any(|pending| same_native_action_identity(pending, &action))
            || self
                .retained_action_overflow
                .as_ref()
                .is_some_and(|pending| same_native_action_identity(pending, &action))
        {
            return true;
        }
        if self.action_lane_len() >= MAX_ACTION_LANE_RECORDS {
            return false;
        }
        if self.pending_host_actions.len() < MAX_RETRY_HOST_ACTIONS {
            self.pending_host_actions.push_back(action);
            true
        } else if self.retained_action_overflow.is_none() {
            self.retained_action_overflow = Some(action);
            true
        } else {
            false
        }
    }

    fn try_enqueue_host_action(&mut self, record: NativeActionRecord) -> NativeHostActionResult {
        match self.host_runtime.as_mut() {
            Some(NativeHostRuntimeAttachment::Client(runtime)) => runtime.enqueue(record),
            Some(NativeHostRuntimeAttachment::Injected(runtime)) => runtime.enqueue(record),
            None => NativeHostActionResult::Disconnected,
        }
    }

    fn set_transport_failure(
        &mut self,
        action: &NativeActionRecord,
        result: NativeHostActionResult,
    ) {
        let failure = match result {
            NativeHostActionResult::Disconnected => NativeHostActionFailure::Disconnected {
                action_id: action.id,
            },
            NativeHostActionResult::QueueFull => NativeHostActionFailure::QueueFull {
                action_id: action.id,
            },
            NativeHostActionResult::Stale => NativeHostActionFailure::Stale {
                action_id: action.id,
                command_id: native_command_id(&action.command),
            },
            NativeHostActionResult::Queued => return,
        };
        self.host_state = match &failure {
            NativeHostActionFailure::Disconnected { .. } => NativeHostState::Disconnected,
            _ => NativeHostState::Error {
                message: bounded_host_error(failure.retry_message()),
            },
        };
        self.last_action_failure = Some(failure);
    }

    fn set_action_capacity_failure(&mut self, action_id: &'static str) {
        let failure = NativeHostActionFailure::QueueFull { action_id };
        self.host_state = NativeHostState::Error {
            message: bounded_host_error(failure.retry_message()),
        };
        self.last_action_failure = Some(failure);
    }

    fn set_execution_failure(
        &mut self,
        action: &NativeActionRecord,
        error: String,
        uncertain: bool,
    ) {
        let command_id = native_command_id(&action.command);
        let failure = if uncertain {
            NativeHostActionFailure::ExecutionUncertain {
                action_id: action.id,
                command_id,
                message: bounded_host_error(error),
            }
        } else {
            NativeHostActionFailure::ExecutionFailed {
                action_id: action.id,
                command_id,
                message: bounded_host_error(error),
            }
        };
        self.host_state = NativeHostState::Error {
            message: bounded_host_error(failure.retry_message()),
        };
        self.last_action_failure = Some(failure);
    }

    fn apply_epoch_fenced_action_outcome(&mut self, outcome: NativeHostActionOutcome) {
        let action = outcome.action().clone();
        if !self.interaction.accepts_action_outcome_record(&action) {
            if self.retain_pending_host_action(action.clone()) {
                self.set_transport_failure(&action, NativeHostActionResult::Stale);
            } else {
                self.set_action_capacity_failure(action.id);
            }
            return;
        }
        self.apply_action_outcome(outcome);
    }

    fn apply_action_outcome(&mut self, outcome: NativeHostActionOutcome) {
        match outcome {
            NativeHostActionOutcome::Accepted { action, receipt } => {
                let action_id = action.id;
                let command_id = native_command_id(&action.command);
                self.last_action_receipt = Some(receipt.clone());
                if let crate::domain::command::CommandReceipt::Rejected { code, .. } = &receipt {
                    let retained = self.retain_pending_host_action(action.clone());
                    self.set_execution_failure(
                        &action,
                        format!("host rejected command: {code:?}"),
                        false,
                    );
                    if !retained {
                        self.set_action_capacity_failure(action.id);
                    }
                    return;
                }
                if let Some(command_id) = command_id {
                    self.pending_host_actions
                        .retain(|action| native_command_id(&action.command) != Some(command_id));
                    if self
                        .retained_action_overflow
                        .as_ref()
                        .is_some_and(|action| {
                            native_command_id(&action.command) == Some(command_id)
                        })
                    {
                        self.retained_action_overflow = None;
                    }
                }
                if self.last_action_failure.as_ref().is_some_and(|failure| {
                    failure.action_id() == action_id && failure.command_id() == command_id
                }) {
                    self.last_action_failure = None;
                    self.restore_connected_host_state();
                }
                if let Some(composer) = self.composer.as_mut() {
                    if let Some(pending) = composer.pending_intent().map(|intent| intent.command_id)
                    {
                        let _ = composer.settle_pending(pending);
                    }
                }
                self.composer_error = None;
            }
            NativeHostActionOutcome::Queried {
                action,
                detail,
                body,
            } => {
                let action_id = action.id;
                let request_id = native_request_id(&action.command);
                self.last_query_detail = Some(detail);
                self.apply_query_body(body);
                if let Some(request_id) = request_id {
                    self.pending_host_actions
                        .retain(|pending| native_request_id(&pending.command) != Some(request_id));
                    if self
                        .retained_action_overflow
                        .as_ref()
                        .is_some_and(|pending| {
                            native_request_id(&pending.command) == Some(request_id)
                        })
                    {
                        self.retained_action_overflow = None;
                    }
                } else {
                    self.pending_host_actions
                        .retain(|pending| !same_native_action_identity(pending, &action));
                    if self
                        .retained_action_overflow
                        .as_ref()
                        .is_some_and(|pending| same_native_action_identity(pending, &action))
                    {
                        self.retained_action_overflow = None;
                    }
                }
                if self
                    .last_action_failure
                    .as_ref()
                    .is_some_and(|failure| failure.action_id() == action_id)
                {
                    self.last_action_failure = None;
                    self.restore_connected_host_state();
                }
            }
            NativeHostActionOutcome::Failed { action, error } => {
                self.composer_error = Some(error.clone());
                let retained = self.retain_pending_host_action(action.clone());
                self.set_execution_failure(&action, error, false);
                if !retained {
                    self.set_action_capacity_failure(action.id);
                }
            }
            NativeHostActionOutcome::Uncertain { action, error } => {
                let retained = self.retain_pending_host_action(action.clone());
                self.set_execution_failure(&action, error, true);
                if !retained {
                    self.set_action_capacity_failure(action.id);
                }
            }
        }
    }

    fn clear_recovered_action_failure(&mut self) {
        if self.pending_host_actions.is_empty()
            && self.retained_action_overflow.is_none()
            && self.last_action_failure.as_ref().is_some_and(|failure| {
                matches!(
                    failure,
                    NativeHostActionFailure::QueueFull { .. }
                        | NativeHostActionFailure::Disconnected { .. }
                        | NativeHostActionFailure::Stale { .. }
                        | NativeHostActionFailure::ExecutionFailed { .. }
                        | NativeHostActionFailure::ExecutionUncertain { .. }
                )
            })
        {
            self.last_action_failure = None;
            self.restore_connected_host_state();
        }
    }

    fn restore_connected_host_state(&mut self) {
        let Some(runtime_state) = self.host_runtime.as_ref().map(|runtime| match runtime {
            NativeHostRuntimeAttachment::Injected(runtime) => runtime.host_state(),
            NativeHostRuntimeAttachment::Client(runtime) => runtime.host_state(),
        }) else {
            self.host_state = NativeHostState::Disconnected;
            return;
        };
        self.host_state = runtime_state;
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
        if self.interaction.sync_host_epochs(runtime_epochs) {
            self.clear_cockpit_projection();
        }
        let mut current_epochs = self.interaction.host_runtime_epochs();
        let mut current_navigation_epoch = self.interaction.action_epochs().navigation_epoch;
        self.rebind_pending_host_actions(current_epochs, current_navigation_epoch);

        // Observe new projections before any caller-requested reconciliation.
        // Failed and uncertain actions remain visible until an explicit
        // reconciliation boundary instead of being hidden by a same-tick
        // loopback.
        let mut projections = match self.host_runtime.as_mut() {
            Some(NativeHostRuntimeAttachment::Injected(runtime)) => {
                runtime.drain_projection_messages(max)
            }
            Some(NativeHostRuntimeAttachment::Client(runtime)) => {
                runtime.take_ready_projection_messages(max)
            }
            None => Vec::new(),
        };
        if let Some(outcome) = self
            .host_runtime
            .as_mut()
            .and_then(|runtime| match runtime {
                NativeHostRuntimeAttachment::Injected(runtime) => {
                    runtime.take_deferred_action_outcome()
                }
                NativeHostRuntimeAttachment::Client(runtime) => {
                    runtime.take_deferred_action_outcome()
                }
            })
        {
            projections.push(NativeHostProjection {
                kind: NativeHostProjectionKind::Error,
                client_model: None,
                error: Some(
                    "native host action outcome retained under projection pressure".to_string(),
                ),
                epochs: Some(current_epochs),
                action_outcome: Some(outcome),
            });
        }
        let had_projections = !projections.is_empty();
        let mut accepted_projection_kinds = Vec::with_capacity(projections.len());
        for projection in projections {
            let stale_projection = projection.epochs.is_some_and(|epochs| {
                if !epochs.is_strictly_older_than(current_epochs) {
                    self.interaction.sync_host_epochs(epochs);
                    current_epochs = self.interaction.host_runtime_epochs();
                    current_navigation_epoch = self.interaction.action_epochs().navigation_epoch;
                    self.rebind_pending_host_actions(current_epochs, current_navigation_epoch);
                    false
                } else {
                    true
                }
            });
            if stale_projection && projection.action_outcome.is_none() {
                continue;
            }
            accepted_projection_kinds.push(projection.kind);
            if !stale_projection {
                if let Some(model) = projection.client_model {
                    if let Err(error) = self.apply_client_model(model) {
                        self.host_state = NativeHostState::Error { message: error };
                    }
                }
            }
            if let Some(outcome) = projection.action_outcome {
                self.apply_epoch_fenced_action_outcome(outcome);
            } else if let Some(error) = projection.error {
                self.host_state = NativeHostState::Error { message: error };
            }
        }
        if had_projections {
            self.last_projection_kinds = accepted_projection_kinds;
        }
        self.admit_ready_stream_frames(max);
        self.sync_top_bar_from_updater();

        for _ in 0..max.min(MAX_PENDING_HOST_ACTIONS) {
            let Some(has_pending) = self.host_runtime.as_ref().map(|runtime| match runtime {
                NativeHostRuntimeAttachment::Injected(runtime) => runtime.pending_front().is_some(),
                NativeHostRuntimeAttachment::Client(runtime) => runtime.pending_front().is_some(),
            }) else {
                break;
            };
            if !has_pending {
                break;
            }

            let pending_action = match self.host_runtime.as_ref() {
                Some(NativeHostRuntimeAttachment::Injected(runtime)) => {
                    runtime.pending_front().cloned()
                }
                Some(NativeHostRuntimeAttachment::Client(runtime)) => {
                    runtime.pending_front().cloned()
                }
                None => None,
            };
            if let Some(action) = pending_action.as_ref() {
                if !self.interaction.accepts_action_record(action) {
                    let stale = match self.host_runtime.as_mut() {
                        Some(NativeHostRuntimeAttachment::Injected(runtime)) => {
                            runtime.take_pending_front()
                        }
                        Some(NativeHostRuntimeAttachment::Client(runtime)) => {
                            runtime.take_pending_front()
                        }
                        None => None,
                    };
                    if let Some(stale) = stale {
                        if self.retain_pending_host_action(stale.clone()) {
                            self.set_transport_failure(&stale, NativeHostActionResult::Stale);
                        } else {
                            self.set_action_capacity_failure(stale.id);
                        }
                    }
                    break;
                }
            }
            let result = match self.host_runtime.as_mut() {
                Some(NativeHostRuntimeAttachment::Injected(runtime)) => {
                    runtime.dispatch_next_pending()
                }
                Some(NativeHostRuntimeAttachment::Client(runtime)) => {
                    runtime.dispatch_next_pending()
                }
                None => break,
            };
            match result {
                NativeHostActionResult::Queued => {
                    self.clear_recovered_action_failure();
                }
                NativeHostActionResult::Disconnected => {
                    if let Some(action) = pending_action.as_ref() {
                        self.set_transport_failure(action, NativeHostActionResult::Disconnected);
                    } else {
                        self.host_state = NativeHostState::Disconnected;
                    }
                    break;
                }
                NativeHostActionResult::QueueFull => {
                    if let Some(action) = pending_action.as_ref() {
                        self.set_transport_failure(action, NativeHostActionResult::QueueFull);
                    } else {
                        self.host_state = NativeHostState::Error {
                            message: bounded_host_error(
                                "native host worker queue is saturated; action retained"
                                    .to_string(),
                            ),
                        };
                    }
                    break;
                }
                NativeHostActionResult::Stale => {
                    if let Some(action) = pending_action.as_ref() {
                        self.set_transport_failure(action, NativeHostActionResult::Stale);
                    }
                    break;
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
                .navigation_mouse_down(task_id, &self.task_list);
        }

        let offset = self.task_scroll_handle.0.borrow().base_handle.offset().y / px(1.0);
        let metrics = self.preferences.tokens().density.physical();
        let first_visible = (offset.max(0.0) / metrics.row_height as f32).floor() as usize;
        let _ = self
            .task_list
            .set_viewport(first_visible, DEFAULT_VISIBLE_ROWS);
        self.accessibility_tree = AccessibilityTree::for_task_list_with_header(
            &self.task_list,
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

    /// Explicit acknowledged detach path used by UI action and ordinary close.
    pub fn request_acknowledged_client_detach(&mut self) -> Result<(), NativeShellError> {
        let deadline = NativeShutdownDeadline::from_now(NATIVE_SHUTDOWN_BUDGET);
        match self.host_runtime.as_mut() {
            Some(NativeHostRuntimeAttachment::Client(runtime)) => {
                runtime.acknowledge_client_detach(deadline)?;
                Ok(())
            }
            Some(NativeHostRuntimeAttachment::Injected(_)) => Err(NativeShellError::HostConnect {
                message: "injected host runtime cannot acknowledge detach".to_string(),
            }),
            None => Err(NativeShellError::HostConnect {
                message: "no host runtime attached for client detach".to_string(),
            }),
        }
    }

    /// Explicit full-quit path using inspect_host_quit then confirm_host_quit.
    pub fn request_full_host_quit(
        &mut self,
        allow_uninspected_worktrees: bool,
    ) -> Result<crate::domain::command::CommandReceipt, NativeShellError> {
        let deadline = NativeShutdownDeadline::from_now(NATIVE_SHUTDOWN_BUDGET);
        match self.host_runtime.as_mut() {
            Some(NativeHostRuntimeAttachment::Client(runtime)) => {
                runtime.confirm_full_host_quit(allow_uninspected_worktrees, deadline)
            }
            Some(NativeHostRuntimeAttachment::Injected(_)) => Err(NativeShellError::HostConnect {
                message: "injected host runtime cannot confirm full quit".to_string(),
            }),
            None => Err(NativeShellError::HostConnect {
                message: "no host runtime attached for full quit".to_string(),
            }),
        }
    }

    pub(crate) fn host_runtime(&self) -> Option<&NativeHostClientRuntime> {
        self.host_runtime
            .as_ref()
            .and_then(|attachment| match attachment {
                NativeHostRuntimeAttachment::Client(runtime) => Some(runtime),
                NativeHostRuntimeAttachment::Injected(_) => None,
            })
    }

    pub(crate) fn host_runtime_mut(&mut self) -> Option<&mut NativeHostClientRuntime> {
        self.host_runtime
            .as_mut()
            .and_then(|attachment| match attachment {
                NativeHostRuntimeAttachment::Client(runtime) => Some(runtime),
                NativeHostRuntimeAttachment::Injected(_) => None,
            })
    }

    fn apply_drained_projection_messages(
        &mut self,
        projections: Vec<NativeHostProjection>,
    ) -> Vec<NativeHostProjectionKind> {
        let runtime_epochs = self
            .host_runtime
            .as_ref()
            .map(|runtime| match runtime {
                NativeHostRuntimeAttachment::Injected(runtime) => runtime.epochs(),
                NativeHostRuntimeAttachment::Client(runtime) => runtime.epochs(),
            })
            .unwrap_or_default();
        if self.interaction.sync_host_epochs(runtime_epochs) {
            self.clear_cockpit_projection();
        }
        let mut current_epochs = self.interaction.host_runtime_epochs();
        let mut current_navigation_epoch = self.interaction.action_epochs().navigation_epoch;
        self.rebind_pending_host_actions(current_epochs, current_navigation_epoch);
        let mut kinds = Vec::with_capacity(projections.len());
        for projection in projections {
            let stale_projection = projection.epochs.is_some_and(|epochs| {
                if !epochs.is_strictly_older_than(current_epochs) {
                    self.interaction.sync_host_epochs(epochs);
                    current_epochs = self.interaction.host_runtime_epochs();
                    current_navigation_epoch = self.interaction.action_epochs().navigation_epoch;
                    self.rebind_pending_host_actions(current_epochs, current_navigation_epoch);
                    false
                } else {
                    true
                }
            });
            if stale_projection && projection.action_outcome.is_none() {
                continue;
            }
            kinds.push(projection.kind);
            if !stale_projection {
                if let Some(model) = projection.client_model {
                    if let Err(error) = self.apply_client_model(model) {
                        self.host_state = NativeHostState::Error { message: error };
                    }
                }
            }
            if let Some(outcome) = projection.action_outcome {
                self.apply_epoch_fenced_action_outcome(outcome);
            } else if let Some(error) = projection.error {
                self.host_state = NativeHostState::Error { message: error };
            }
        }
        kinds
    }

    /// Drain only the injected/controller-owned projection queue. The method
    /// is deliberately explicit so GPUI paint/input callbacks cannot perform
    /// transport work. Full projection messages are applied before returning
    /// their kinds so action receipts/outcomes are never discarded.
    pub fn drain_host_projections(&mut self, max: usize) -> Vec<NativeHostProjectionKind> {
        let mut projections = match self.host_runtime.as_mut() {
            Some(NativeHostRuntimeAttachment::Injected(runtime)) => {
                runtime.drain_projection_messages(max)
            }
            Some(NativeHostRuntimeAttachment::Client(runtime)) => {
                runtime.take_ready_projection_messages(max)
            }
            None => Vec::new(),
        };
        if let Some(outcome) = self
            .host_runtime
            .as_mut()
            .and_then(|runtime| match runtime {
                NativeHostRuntimeAttachment::Injected(runtime) => {
                    runtime.take_deferred_action_outcome()
                }
                NativeHostRuntimeAttachment::Client(runtime) => {
                    runtime.take_deferred_action_outcome()
                }
            })
        {
            projections.push(NativeHostProjection {
                kind: NativeHostProjectionKind::Error,
                client_model: None,
                error: Some(
                    "native host action outcome retained under projection pressure".to_string(),
                ),
                epochs: None,
                action_outcome: Some(outcome),
            });
        }
        self.apply_drained_projection_messages(projections)
    }

    /// Controller-lane async drain for the single real client. This is kept
    /// separate from synchronous paint/input APIs so a GPUI callback cannot
    /// accidentally await transport work.
    pub async fn drain_host_projections_async(
        &mut self,
        max: usize,
    ) -> Result<Vec<NativeHostProjectionKind>, IpcError> {
        let mut projections = match self.host_runtime.as_mut() {
            Some(NativeHostRuntimeAttachment::Client(runtime)) => {
                runtime.take_ready_projection_messages(max)
            }
            Some(NativeHostRuntimeAttachment::Injected(runtime)) => {
                runtime.drain_projection_messages(max)
            }
            None => Vec::new(),
        };
        if let Some(outcome) = self
            .host_runtime
            .as_mut()
            .and_then(|runtime| match runtime {
                NativeHostRuntimeAttachment::Injected(runtime) => {
                    runtime.take_deferred_action_outcome()
                }
                NativeHostRuntimeAttachment::Client(runtime) => {
                    runtime.take_deferred_action_outcome()
                }
            })
        {
            projections.push(NativeHostProjection {
                kind: NativeHostProjectionKind::Error,
                client_model: None,
                error: Some(
                    "native host action outcome retained under projection pressure".to_string(),
                ),
                epochs: None,
                action_outcome: Some(outcome),
            });
        }
        Ok(self.apply_drained_projection_messages(projections))
    }

    /// Attach exactly one pre-connected host runtime. The shell never opens
    /// another connection when an attachment is present.
    pub(crate) fn attach_host_runtime(
        &mut self,
        host_runtime: NativeHostClientRuntime,
    ) -> Result<(), NativeHostClientRuntime> {
        if self.host_runtime.is_some() {
            return Err(host_runtime);
        }
        if let Err(error) = host_runtime.validate_attachment(&self.profile) {
            self.host_state = NativeHostState::Error {
                message: bounded_host_error(error.to_string()),
            };
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

    pub fn platform_accessibility_available(&self) -> bool {
        self.platform_accessibility.is_available()
    }

    pub fn platform_accessibility_node_count(&self) -> usize {
        self.platform_accessibility.node_count()
    }

    /// Replace the bounded host projection supplied by the inbox attachment.
    /// This is a pure handoff; no client, subscription, or second connection
    /// is created by the shell.
    fn apply_task_list(&mut self, task_list: TaskList) {
        let selected_task = self
            .interaction
            .selected_task()
            .filter(|task_id| task_list.task_ids().contains(task_id));
        self.interaction.sync_selected_task(selected_task);
        self.task_list = task_list;
        self.accessibility_tree = AccessibilityTree::for_task_list_with_header(
            &self.task_list,
            self.interaction.selected_task(),
            &self.header_attachment,
        );
        self.platform_accessibility.sync(&self.accessibility_tree);
    }

    pub fn apply_client_model(&mut self, model: Arc<ClientModel>) -> Result<(), String> {
        let task_list = TaskList::from_client_model_virtual(&model)
            .map_err(|error| format!("client model task projection failed: {error:?}"))?;
        self.apply_task_list(task_list);
        self.inbox = Inbox::from_model(&model);
        self.client_model = Some(Arc::clone(&model));
        self.interaction.bind_projected_model(Arc::clone(&model));
        self.services_projection = project_services_panel(&[], &[]);
        self.sync_header_projection();
        let client_epoch = self.interaction.action_epochs().client_epoch;
        self.rebind_pending_client_epoch(client_epoch);
        self.sync_cockpit_follow();
        Ok(())
    }

    pub fn inbox_render_model(&self, width: InboxPresentationWidth) -> InboxRenderModel {
        self.inbox.render_model(width)
    }

    pub fn select_projected_task(&mut self, task_id: TaskId) -> NavigationHandlerOutcome {
        let outcome = self
            .interaction
            .navigation_mouse_down(task_id, &self.task_list);
        self.sync_cockpit_follow();
        outcome
    }

    fn sync_header_projection(&mut self) {
        let attachment = self
            .client_model
            .as_ref()
            .and_then(|model| self.interaction.task_header(model.as_ref()))
            .map(|header| {
                let workspace = match &header.workspace {
                    crate::ui::task_cockpit::WorkspaceProjection::Main => "main".to_string(),
                    crate::ui::task_cockpit::WorkspaceProjection::Worktree { branch, .. } => {
                        format!("worktree · {}", bounded_header_text(branch.clone()))
                    }
                    crate::ui::task_cockpit::WorkspaceProjection::External { .. } => {
                        "external".to_string()
                    }
                };
                let remote = match &header.primary {
                    crate::ui::task_cockpit::PrimaryAgentProjection::Present(agent) => {
                        agent.label.clone()
                    }
                    crate::ui::task_cockpit::PrimaryAgentProjection::Unavailable {
                        label, ..
                    } => label.clone(),
                };
                let quota = self
                    .top_bar
                    .as_ref()
                    .map(|controller| {
                        let model = controller.model();
                        if model.quotas.is_empty() {
                            "quota unavailable".to_string()
                        } else {
                            model
                                .quotas
                                .iter()
                                .map(|quota| format!("{} {}", quota.provider, quota.detail))
                                .collect::<Vec<_>>()
                                .join(" · ")
                        }
                    })
                    .unwrap_or_else(|| "quota unavailable".to_string());
                NativeHeaderAttachment::projection(
                    header.title,
                    format!(
                        "{} · {} · rev {}",
                        header.status.label, workspace, header.identity.revision
                    ),
                    format!("Host · {} · {remote}", self.host_state.label()),
                    format!("{} · {}", header.accessible_description, quota),
                )
            })
            .unwrap_or_else(|| NativeHeaderAttachment::unavailable("select a task"));
        if self.header_attachment != attachment {
            self.attach_header_projection(attachment);
        }
    }

    fn sync_cockpit_follow(&mut self) {
        let Some(task_id) = self.interaction.selected_task() else {
            self.clear_cockpit_projection();
            return;
        };
        self.cockpit.follow_task(task_id);
        if let Some(model) = self.client_model.as_ref() {
            self.cockpit.follow_projection(model.as_ref());
        }
        self.refresh_selected_cockpit_surfaces();
        self.sync_terminal_from_cockpit();
        self.sync_header_projection();
    }

    fn clear_cockpit_projection(&mut self) {
        self.cockpit = TaskCockpitShell::new(DockEdge::Bottom);
        self.terminal.rebind(None);
        self.terminal.set_preferences(self.preferences);
        self.sync_header_projection();
    }

    fn sync_terminal_from_cockpit(&mut self) {
        let preferences = self.preferences;
        if self.cockpit.dock().terminal_binding().is_some() {
            let model = self.cockpit.dock().terminal_pane_model();
            self.terminal.rebind(Some(model));
            self.terminal.set_preferences(preferences);
        } else {
            self.terminal.rebind(None);
            self.terminal.set_preferences(preferences);
        }
    }

    fn cockpit_dock_tool(tool: DockTool) -> CockpitDockTool {
        match tool {
            DockTool::Changes => CockpitDockTool::Changes,
            DockTool::Files => CockpitDockTool::Files,
            DockTool::Terminal => CockpitDockTool::Terminal,
            DockTool::Browser => CockpitDockTool::Browser,
            DockTool::Services => CockpitDockTool::Services,
            DockTool::Artifacts => CockpitDockTool::Artifacts,
            DockTool::Review => CockpitDockTool::Review,
        }
    }

    fn apply_keyboard_shell_effects(&mut self, action: KeyboardAction) {
        match action {
            KeyboardAction::SelectDock(tool) => {
                let _ = self
                    .cockpit
                    .handle_tool_action(Self::cockpit_dock_tool(tool), RequestId::new());
                self.sync_terminal_from_cockpit();
                if matches!(tool, DockTool::Services) {
                    // Refresh the shared host/catalog seam when the services
                    // panel becomes active; the panel itself never mints a
                    // supervisor command or bypasses ServiceControl fences.
                    let _ = self.dispatch_action(ActionRequest::HostActions);
                } else if matches!(
                    tool,
                    DockTool::Changes | DockTool::Files | DockTool::Services
                ) {
                    self.refresh_selected_cockpit_surfaces();
                } else if !matches!(tool, DockTool::Terminal) {
                    if let Some(task_id) = self.interaction.selected_task() {
                        let _ = self.dispatch_action(ActionRequest::TaskShow { task_id });
                    }
                }
            }
            KeyboardAction::OpenTerminal => {
                let _ = self
                    .cockpit
                    .handle_tool_action(CockpitDockTool::Terminal, RequestId::new());
                let _ = self.cockpit.handle_toggle_raw(RequestId::new());
                self.sync_terminal_from_cockpit();
            }
            KeyboardAction::OpenTaskDetails => {
                if let Some(task_id) = self.interaction.selected_task() {
                    let _ = self.dispatch_action(ActionRequest::TaskShow { task_id });
                }
            }
            KeyboardAction::OpenCommandPalette => {
                let _ = self
                    .prompt_library
                    .handle_key(PromptLibraryKey::LibraryShortcut);
                self.hydrate_prompt_library();
            }
            KeyboardAction::OpenPalette => {
                let _ = self.prompt_library.handle_key(PromptLibraryKey::Slash);
            }
            _ => {}
        }
    }

    fn task_row_label(&self, task_id: TaskId) -> String {
        if let Some(row) = self.inbox.row(task_id) {
            format!("{} · {}", row.title, visible_status_label(row.status))
        } else if let Some(model) = self.client_model.as_ref() {
            model
                .tasks()
                .get(&task_id)
                .map(|snapshot| {
                    format!(
                        "{} · {}",
                        snapshot.task.title,
                        visible_status_label(snapshot.visible_status())
                    )
                })
                .unwrap_or_else(|| format!("Task {task_id}"))
        } else {
            format!("Task {task_id}")
        }
    }

    fn selected_browser_dock_model(&self) -> Option<TaskBrowserDockModel> {
        let task_id = self.interaction.selected_task()?;
        let model = self.client_model.as_ref()?;
        let view = model.browser_dock_view(task_id)?;
        let tab_labels = (0..view.tab_count)
            .take(32)
            .map(|index| format!("Browser tab {}", index + 1))
            .collect();
        Some(TaskBrowserDockModel {
            task_title: view.title.clone(),
            address: view.shareable_url.unwrap_or_default(),
            title: view.title,
            security: BrowserSecurityState::Unknown,
            loading: false,
            error: None,
            progress: None,
            tab_labels,
            selected_tab: None,
            diagnostic: Some(format!(
                "{} browser tab(s) · context={} · resource={} · agent={} · generation={}",
                view.tab_count,
                view.context_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unbound".to_string()),
                view.resource_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unbound".to_string()),
                view.agent_session_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unbound".to_string()),
                view.generation
                    .map(|generation| generation.to_string())
                    .unwrap_or_else(|| "unbound".to_string()),
            )),
            approval: None,
            artifact_count: model.artifact_summaries().len(),
        })
    }

    fn prompt_library_surface(&self, tokens: crate::ui::tokens::ThemeTokens) -> AnyElement {
        let list = self.prompt_library.list_state();
        let load = match &self.prompt_library.load {
            crate::ui::prompts::PromptLibraryLoadState::Empty => "empty",
            crate::ui::prompts::PromptLibraryLoadState::Loading => "loading",
            crate::ui::prompts::PromptLibraryLoadState::Ready => "ready",
            crate::ui::prompts::PromptLibraryLoadState::Error { .. } => "error",
            crate::ui::prompts::PromptLibraryLoadState::StaleRevision { .. } => "stale",
        };
        let draft_chars = self.prompt_library.draft.text.chars().count();
        let next = self
            .prompt_library
            .suggested_next
            .as_ref()
            .map(|next| format!("next prompt · {}", next.title))
            .unwrap_or_else(|| "next prompt · none".to_string());
        let composer = match (
            self.composer
                .as_ref()
                .and_then(TaskComposer::pending_intent),
            self.composer_error.as_deref(),
        ) {
            (_, Some(error)) => format!("Composer · error · {error}"),
            (Some(intent), None) => format!("Composer · pending · {}", intent.action_id),
            (None, None) => format!(
                "Composer · {} character draft · {}",
                draft_chars,
                if self.prompt_library.draft.sent {
                    "sent"
                } else {
                    "ready"
                }
            ),
        };
        let updater = self
            .updater_snapshot
            .as_ref()
            .map(|snapshot| format!("Updater · {:?}", snapshot.stage))
            .unwrap_or_else(|| "Updater · snapshot unavailable".to_string());
        div()
            .id("native-shell-prompt-composer")
            .w_full()
            .flex()
            .flex_wrap()
            .gap(px(tokens.density.spacing.sm))
            .p(px(tokens.density.physical().control_padding as f32))
            .bg(tokens.surfaces.raised.to_gpui())
            .child(format!(
                "{} · {} · {} saved · {}",
                self.prompt_library.chrome.rail_label,
                self.prompt_library.chrome.active_section.label(),
                list.total,
                load
            ))
            .child(next)
            .child(composer)
            .child(updater)
            .into_any_element()
    }

    fn workspace_dock_surface(
        &self,
        tool: CockpitDockTool,
        tokens: crate::ui::tokens::ThemeTokens,
    ) -> AnyElement {
        let details = if let Some(projection) = self.cockpit.live_projection() {
            let kind = match tool {
                CockpitDockTool::Changes => Some(CockpitSurfaceKind::Git),
                CockpitDockTool::Files => Some(CockpitSurfaceKind::Files),
                CockpitDockTool::Services => Some(CockpitSurfaceKind::Services),
                _ => None,
            };
            if let Some(kind) = kind {
                summary_line(projection, kind)
            } else {
                self.interaction
                    .selected_task()
                    .and_then(|task_id| self.client_model.as_ref()?.task(task_id))
                    .map(|snapshot| match tool {
                        CockpitDockTool::Artifacts => format!(
                            "Artifacts · {} bounded metadata item(s) · task-owned",
                            snapshot.artifacts.len()
                        ),
                        CockpitDockTool::Review => format!(
                            "Review · {} · revision {}",
                            visible_status_label(snapshot.visible_status()),
                            snapshot.task.revision
                        ),
                        other => other.label().to_string(),
                    })
                    .unwrap_or_else(|| format!("{} · select a task", tool.label()))
            }
        } else {
            self.interaction
                .selected_task()
                .and_then(|task_id| self.client_model.as_ref()?.task(task_id))
                .map(|snapshot| {
                    let workspace = workspace_projection_label(&snapshot.task.workspace);
                    match tool {
                        CockpitDockTool::Changes => {
                            format!("Git changes · {workspace} · loading host projection")
                        }
                        CockpitDockTool::Files => {
                            format!("Files · {workspace} · loading host projection")
                        }
                        CockpitDockTool::Artifacts => format!(
                            "Artifacts · {} bounded metadata item(s) · task-owned",
                            snapshot.artifacts.len()
                        ),
                        CockpitDockTool::Review => format!(
                            "Review · {} · revision {}",
                            visible_status_label(snapshot.visible_status()),
                            snapshot.task.revision
                        ),
                        other => other.label().to_string(),
                    }
                })
                .unwrap_or_else(|| format!("{} · select a task", tool.label()))
        };
        div()
            .id("native-shell-workspace-dock")
            .w_full()
            .p(px(tokens.density.physical().control_padding as f32))
            .bg(tokens.surfaces.sunken.to_gpui())
            .child(details)
            .into_any_element()
    }

    fn service_action_request(
        &self,
        task_id: TaskId,
        service_id: &crate::services::model::ServiceId,
        action: ServicePanelAction,
        epochs: NativeActionEpochs,
    ) -> Option<ActionRequest> {
        let arguments = crate::client::action::ServiceControlArguments {
            service_id: service_id.clone(),
            resource_generation: epochs.resource_generation,
            connection_epoch: epochs.connection_epoch,
            action_epoch: epochs.action_epoch,
        };
        match action {
            ServicePanelAction::Start => Some(ActionRequest::ServiceControl {
                action: crate::domain::command::ServiceControlAction::Start,
                arguments,
            }),
            ServicePanelAction::Stop => Some(ActionRequest::ServiceControl {
                action: crate::domain::command::ServiceControlAction::Stop,
                arguments,
            }),
            ServicePanelAction::Restart => Some(ActionRequest::ServiceControl {
                action: crate::domain::command::ServiceControlAction::Restart,
                arguments,
            }),
            ServicePanelAction::Logs | ServicePanelAction::Health => {
                let service_id =
                    crate::domain::id::ConfiguredServiceId::new(service_id.as_str()).ok()?;
                let query = if action == ServicePanelAction::Logs {
                    TaskCockpitQuery::ServiceLogs {
                        service_id,
                        resource_generation: epochs.resource_generation,
                        connection_epoch: epochs.connection_epoch,
                        action_epoch: epochs.action_epoch,
                    }
                } else {
                    TaskCockpitQuery::ServiceHealth {
                        service_id,
                        resource_generation: epochs.resource_generation,
                        connection_epoch: epochs.connection_epoch,
                        action_epoch: epochs.action_epoch,
                    }
                };
                Some(ActionRequest::TaskCockpit { task_id, query })
            }
            ServicePanelAction::OpenTerminal => None,
        }
    }

    fn service_action_is_current(
        &self,
        task_id: TaskId,
        service_id: &crate::services::model::ServiceId,
        epochs: NativeActionEpochs,
    ) -> bool {
        self.interaction.selected_task() == Some(task_id)
            && self.interaction.action_epochs() == epochs
            && self
                .services_projection
                .rows
                .iter()
                .any(|row| row.service_id == *service_id)
    }

    fn services_dock_surface(
        &self,
        tokens: crate::ui::tokens::ThemeTokens,
        shell_entity: Option<gpui::WeakEntity<NativeShell>>,
    ) -> AnyElement {
        let epochs = self.interaction.action_epochs();
        let task_id = self.interaction.selected_task();
        let rows = self
            .services_projection
            .rows
            .iter()
            .map(|row| {
                let tone = match row.tone {
                    ServicePanelTone::Blue => StatusMeaning::External,
                    ServicePanelTone::Green => StatusMeaning::Success,
                    ServicePanelTone::Orange => StatusMeaning::Warning,
                    ServicePanelTone::Red => StatusMeaning::Destructive,
                    ServicePanelTone::Gray | ServicePanelTone::Neutral => {
                        StatusMeaning::Inactive
                    }
                };
                let state_label = format!("{:?}", row.state);
                let description = format!(
                    "{} · {} · {}",
                    row.ownership_summary, row.health_summary, row.dependency_summary
                );
                let status = if row.tone == ServicePanelTone::Blue {
                    let port = row
                        .port_label
                        .clone()
                        .unwrap_or_else(|| "external listener".to_string());
                    StatusLight::external_port(
                        ExternalPortStatus::new(
                            format!("{} · {}", row.service_id, port),
                            description.clone(),
                        )
                        .expect("projected external service status is bounded"),
                    )
                    .expect("projected external service status is bounded")
                } else {
                    StatusLight::new(tone, state_label.clone(), description.clone())
                        .expect("projected service status is bounded")
                };
                let service_id = row.service_id.clone();
                let controls = row
                    .actions
                    .iter()
                    .map(|affordance| {
                        let label = service_panel_action_label(affordance.action);
                        let action = affordance.action;
                        let mut control = div()
                            .id((
                                "native-service-control",
                                stable_service_element_key(
                                    service_id.as_str(),
                                    &label.to_ascii_lowercase(),
                                ),
                            ))
                            .px(px(tokens.density.spacing.sm))
                            .py(px(tokens.density.spacing.xs))
                            .rounded_sm()
                            .border_1()
                            .border_color(tokens.borders.subtle.to_gpui())
                            .child(label);
                        if affordance.enabled {
                            if let Some(task_id) = task_id {
                                if let Some(shell_entity) = shell_entity.clone() {
                                    let service_id_for_action = service_id.clone();
                                    control = control
                                        .cursor_pointer()
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            move |event, _window, app| {
                                                if event.button != MouseButton::Left {
                                                    return;
                                                }
                                                let service_id = service_id_for_action.clone();
                                                let _ = shell_entity.update(app, |shell, cx| {
                                                    cx.stop_propagation();
                                                    if !shell.service_action_is_current(
                                                        task_id,
                                                        &service_id,
                                                        epochs,
                                                    ) {
                                                        shell.last_query_detail = Some(
                                                            "Service action expired; refresh the host projection and try again."
                                                                .to_string(),
                                                        );
                                                        return;
                                                    }
                                                    shell
                                                        .interaction
                                                        .begin_control_pointer(NATIVE_POINTER_ID);
                                                    if let Some(request) = shell
                                                        .service_action_request(
                                                            task_id,
                                                            &service_id,
                                                            action,
                                                            epochs,
                                                        )
                                                    {
                                                        let _ = shell.dispatch_pointer_action(
                                                            request,
                                                            NATIVE_POINTER_ID,
                                                        );
                                                    }
                                                    shell
                                                        .interaction
                                                        .release_pointer(NATIVE_POINTER_ID);
                                                });
                                            },
                                        );
                                }
                            }
                        } else {
                            control = control
                                .text_color(tokens.text.disabled.to_gpui())
                                .child(
                                    affordance
                                        .disabled_reason
                                        .unwrap_or("Unavailable"),
                                );
                        }
                        control.into_any_element()
                    })
                    .collect::<Vec<_>>();
                div()
                    .id((
                        "native-service-row",
                        stable_service_element_key(row.service_id.as_str(), "row"),
                    ))
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(tokens.density.spacing.sm))
                    .p(px(tokens.density.physical().row_padding as f32))
                    .border_b_1()
                    .border_color(tokens.borders.subtle.to_gpui())
                    .child(status.element(tokens))
                    .child(
                        div()
                            .flex_col()
                            .flex_grow()
                            .child(row.service_id.to_string())
                            .child(description),
                    )
                    .children(controls)
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let body = if rows.is_empty() {
            div()
                .text_color(tokens.text.secondary.to_gpui())
                .child("No configured services in the selected task.")
                .into_any_element()
        } else {
            div().flex_col().children(rows).into_any_element()
        };
        div()
            .id("native-shell-services-dock")
            .w_full()
            .p(px(tokens.density.physical().control_padding as f32))
            .bg(tokens.surfaces.sunken.to_gpui())
            .child(body)
            .into_any_element()
    }

    fn context_dock_surface(
        &self,
        tokens: crate::ui::tokens::ThemeTokens,
        shell_entity: Option<gpui::WeakEntity<NativeShell>>,
    ) -> AnyElement {
        match self.cockpit.active_tool() {
            CockpitDockTool::Terminal => self
                .cockpit
                .dock()
                .render_context_dock(tokens)
                .into_any_element(),
            CockpitDockTool::Browser => self
                .selected_browser_dock_model()
                .map(|model| render_task_browser_dock(model, tokens).into_any_element())
                .unwrap_or_else(|| self.workspace_dock_surface(CockpitDockTool::Browser, tokens)),
            CockpitDockTool::Services => self.services_dock_surface(tokens, shell_entity),
            tool => self.workspace_dock_surface(tool, tokens),
        }
    }

    pub fn client_model_snapshot(&self) -> Option<Arc<ClientModel>> {
        self.client_model.clone()
    }

    pub fn task_list(&self) -> &TaskList {
        &self.task_list
    }

    pub fn inbox(&self) -> &Inbox {
        &self.inbox
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
        let details = self
            .interaction
            .keyboard_state()
            .task_details_open
            .then(|| {
                let selected = self.interaction.selected_task();
                let body = selected
                    .map(|task_id| self.task_row_label(task_id))
                    .unwrap_or_else(|| "No task selected".to_string());
                div()
                    .id("native-shell-task-details")
                    .w_full()
                    .p(px(metrics.control_padding as f32))
                    .bg(tokens.surfaces.raised.to_gpui())
                    .child(format!("Task details · {body}"))
            });
        let prompt_composer = self.prompt_library_surface(tokens);
        let context_dock = div()
            .id("native-shell-context-dock")
            .w_full()
            .child(self.context_dock_surface(tokens, None));
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
            .children(details)
            .child(prompt_composer)
            .child(context_dock)
            .child(terminal)
    }

    fn element_with_handlers(&mut self, cx: &Context<Self>) -> impl IntoElement {
        let tokens = self.preferences.tokens();
        let metrics = tokens.density.physical();
        let task_ids = Arc::new(self.task_list.task_ids().to_vec());
        let task_labels = Arc::new(
            task_ids
                .iter()
                .map(|task_id| (*task_id, self.task_row_label(*task_id)))
                .collect::<std::collections::HashMap<_, _>>(),
        );
        let shell_entity = cx.entity().downgrade();
        let services_shell_entity = shell_entity.clone();
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
                        let row_label = task_labels
                            .get(&task_id)
                            .cloned()
                            .unwrap_or_else(|| format!("Task {task_id}"));
                        let mouse_handler =
                            move |event: &MouseDownEvent,
                                  window: &mut Window,
                                  app: &mut gpui::App| {
                                let _ = shell_for_mouse.update(app, |shell, cx| {
                                    cx.stop_propagation();
                                    if event.button == MouseButton::Left {
                                        shell.focus_handle.focus(window);
                                        let _ = shell
                                            .interaction
                                            .navigation_mouse_down(task_id, &shell.task_list);
                                        shell.sync_cockpit_follow();
                                        shell.accessibility_tree =
                                            AccessibilityTree::for_task_list_with_header(
                                                &shell.task_list,
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
                                let _ = shell_for_mouse_up.update(app, |shell, cx| {
                                    cx.stop_propagation();
                                    shell.interaction.release_pointer(NATIVE_POINTER_ID);
                                });
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
                                        shell.sync_cockpit_follow();
                                        shell.accessibility_tree =
                                            AccessibilityTree::for_task_list_with_header(
                                                &shell.task_list,
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
                            .child(row_label)
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
                &shell.task_list,
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
        let client_detach = cx.listener(|shell, _action: &NativeClientDetach, _window, cx| {
            cx.stop_propagation();
            match shell.request_acknowledged_client_detach() {
                Ok(_) => cx.quit(),
                Err(error) => {
                    shell.last_action_failure = Some(NativeHostActionFailure::ExecutionFailed {
                        action_id: "native.client_detach",
                        command_id: None,
                        message: bounded_host_error(error.to_string()),
                    });
                }
            }
        });
        let host_full_quit = cx.listener(|shell, _action: &NativeHostFullQuit, _window, cx| {
            cx.stop_propagation();
            // Explicit UI full quit authorizes the NotInspected worktree path;
            // agent/resource blockers still fail closed via inspect facts.
            match shell.request_full_host_quit(true) {
                Ok(_) => cx.quit(),
                Err(error) => {
                    shell.last_action_failure = Some(NativeHostActionFailure::ExecutionFailed {
                        action_id: "native.host_full_quit",
                        command_id: None,
                        message: bounded_host_error(error.to_string()),
                    });
                }
            }
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
            .on_action::<NativeClientDetach>(client_detach)
            .on_action::<NativeHostFullQuit>(host_full_quit)
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
                        div()
                            .id("native-shell-host-status-hit")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|shell, _event: &MouseDownEvent, _window, cx| {
                                    cx.stop_propagation();
                                    shell.interaction.begin_control_pointer(NATIVE_POINTER_ID);
                                }),
                            )
                            .child(
                                Button::new("native-shell-host-status")
                                    .label("Host status")
                                    .info()
                                    .on_click(cx.listener(
                                        |shell, _event: &ClickEvent, _window, cx| {
                                            cx.stop_propagation();
                                            shell.dispatch_pointer_action(
                                                ActionRequest::HostStatus,
                                                NATIVE_POINTER_ID,
                                            );
                                        },
                                    )),
                            ),
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
            .children(
                self.interaction
                    .keyboard_state()
                    .task_details_open
                    .then(|| {
                        let selected = self.interaction.selected_task();
                        let body = selected
                            .map(|task_id| self.task_row_label(task_id))
                            .unwrap_or_else(|| "No task selected".to_string());
                        div()
                            .id("native-shell-task-details")
                            .w_full()
                            .p(px(metrics.control_padding as f32))
                            .bg(tokens.surfaces.raised.to_gpui())
                            .child(format!("Task details · {body}"))
                    }),
            )
            .child(div().w_full().child(self.prompt_library_surface(tokens)))
            .child(
                div()
                    .id("native-shell-context-dock")
                    .w_full()
                    .child(self.context_dock_surface(tokens, Some(services_shell_entity))),
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

    pub fn dispatch_action_for_test(
        &mut self,
        request: ActionRequest,
    ) -> Option<NativeActionRecord> {
        self.dispatch_action(request)
    }

    #[cfg(test)]
    pub fn dispatch_keyboard_for_test(&mut self, shortcut: KeyboardShortcut) {
        self.dispatch_keyboard(shortcut);
    }

    #[cfg(test)]
    pub fn task_row_label_for_test(&self, task_id: TaskId) -> String {
        self.task_row_label(task_id)
    }

    fn dispatch_action(&mut self, request: ActionRequest) -> Option<NativeActionRecord> {
        if self.action_lane_len() >= MAX_ACTION_LANE_RECORDS {
            // The shell-side retry queue is deliberately bounded. Refuse a
            // new intent before constructing a record when that bound is
            // reached, retaining every already-created record and surfacing
            // typed pressure to the user instead of silently evicting one.
            self.set_action_capacity_failure(request.id());
            return None;
        }
        let Some(record) = self.interaction.action(request) else {
            return None;
        };
        let returned = record.clone();
        if let NativeHostCommand::Browser(command) = record.command.clone() {
            return match self.apply_browser_native_command(&command) {
                Ok(_) => None,
                Err(_) => Some(returned),
            };
        }
        match self.enqueue_host_action(record) {
            NativeHostActionResult::Queued => None,
            NativeHostActionResult::Disconnected
            | NativeHostActionResult::QueueFull
            | NativeHostActionResult::Stale => Some(returned),
        }
    }

    #[cfg(test)]
    fn dispatch_pointer_action_for_test(
        &mut self,
        request: ActionRequest,
        pointer_id: u64,
    ) -> Option<NativeActionRecord> {
        self.dispatch_pointer_action(request, pointer_id)
    }

    fn dispatch_pointer_action(
        &mut self,
        request: ActionRequest,
        pointer_id: u64,
    ) -> Option<NativeActionRecord> {
        if self.action_lane_len() >= MAX_ACTION_LANE_RECORDS {
            self.set_action_capacity_failure(request.id());
            return None;
        }
        let Some(record) = self
            .interaction
            .action_from_source(request, ActivationSource::Pointer { pointer_id })
        else {
            return None;
        };
        let returned = record.clone();
        if let NativeHostCommand::Browser(command) = record.command.clone() {
            return match self.apply_browser_native_command(&command) {
                Ok(_) => None,
                Err(_) => Some(returned),
            };
        }
        match self.enqueue_host_action(record) {
            NativeHostActionResult::Queued => None,
            NativeHostActionResult::Disconnected
            | NativeHostActionResult::QueueFull
            | NativeHostActionResult::Stale => Some(returned),
        }
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
            self.apply_keyboard_shell_effects(action);
        }
    }

    fn enqueue_host_action(&mut self, record: NativeActionRecord) -> NativeHostActionResult {
        if !self.interaction.accepts_action_record(&record) {
            let result = NativeHostActionResult::Stale;
            if self.retain_pending_host_action(record.clone()) {
                self.set_transport_failure(&record, result);
            } else {
                self.set_action_capacity_failure(record.id);
            }
            return result;
        }
        let result = self.try_enqueue_host_action(record.clone());
        match result {
            NativeHostActionResult::Queued => {}
            NativeHostActionResult::Disconnected
            | NativeHostActionResult::QueueFull
            | NativeHostActionResult::Stale => {
                if self.retain_pending_host_action(record.clone()) {
                    self.set_transport_failure(&record, result);
                } else {
                    self.set_action_capacity_failure(record.id);
                }
            }
        }
        result
    }

    fn host_status_text(&self) -> String {
        let base = match &self.host_state {
            NativeHostState::Connected { .. } => "Connected · host".to_string(),
            NativeHostState::Disconnected => "Disconnected · host".to_string(),
            NativeHostState::Error { .. } => "Error · host".to_string(),
        };
        let with_failure = self
            .last_action_failure
            .as_ref()
            .map_or(base.clone(), |failure| {
                format!("{base} · {}", failure.retry_message())
            });
        match self.last_query_detail.as_ref() {
            Some(detail) => format!("{with_failure} · {detail}"),
            None => with_failure,
        }
    }
}

impl Render for NativeShell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.element_with_handlers(cx)
    }
}

/// Launch the native GPUI shell.
///
/// Debug builds use the generated isolated workspace profile and may parent-bind
/// the host. Release builds use the fail-closed production profile and attach to
/// the durable sibling host without parent-pid ownership.
pub fn run_native_shell(workspace_root: impl AsRef<Path>) -> Result<(), NativeShellError> {
    #[cfg(debug_assertions)]
    {
        let profile = isolated_dev_profile(workspace_root)?;
        let mut bootstrap = ProcessNativeHostBootstrap;
        run_native_shell_with_bootstrap(profile, &mut bootstrap)
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = workspace_root;
        let profile = production_shell_profile()?;
        let mut bootstrap = ProcessNativeHostBootstrap;
        run_native_shell_with_bootstrap(profile, &mut bootstrap)
    }
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
pub(crate) fn run_native_shell_with_runtime(
    workspace_root: impl AsRef<Path>,
    host_runtime: Option<NativeHostClientRuntime>,
) -> Result<(), NativeShellError> {
    let profile = isolated_dev_profile(workspace_root)?;
    let (host_runtime, host_state) = validated_runtime_attachment(&profile, host_runtime);
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
        "devmanager native shell profile: {}",
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

fn bounded_host_error(message: impl Into<String>) -> String {
    const MAX_HOST_ERROR_CHARS: usize = 256;
    message.into().chars().take(MAX_HOST_ERROR_CHARS).collect()
}

fn workspace_projection_label(workspace: &crate::domain::task::WorkspaceRef) -> String {
    use crate::domain::task::{WorkspaceBindingKind, WorkspaceRef};

    match workspace {
        WorkspaceRef::Main | WorkspaceRef::MainWithFingerprint { .. } => "main".to_string(),
        WorkspaceRef::Worktree { branch, .. }
        | WorkspaceRef::WorktreeWithFingerprint { branch, .. } => {
            format!("worktree · {}", bounded_header_text(branch.clone()))
        }
        WorkspaceRef::External { .. } | WorkspaceRef::ExternalWithFingerprint { .. } => {
            "external".to_string()
        }
        WorkspaceRef::HostBound { binding } => match binding.kind() {
            WorkspaceBindingKind::Main => "main · host-bound".to_string(),
            WorkspaceBindingKind::Worktree => format!(
                "worktree · {} · host-bound",
                binding
                    .branch()
                    .map(|branch| bounded_header_text(branch.to_string()))
                    .unwrap_or_else(|| "branch unavailable".to_string())
            ),
            WorkspaceBindingKind::External => "external · host-bound".to_string(),
        },
    }
}

fn visible_status_label(status: VisibleTaskStatus) -> &'static str {
    match status {
        VisibleTaskStatus::Disconnected => "Disconnected",
        VisibleTaskStatus::Failed => "Failed",
        VisibleTaskStatus::UncertainOutcome => "Uncertain",
        VisibleTaskStatus::NeedsApproval => "Needs approval",
        VisibleTaskStatus::NeedsAnswer => "Needs answer",
        VisibleTaskStatus::Working => "Working",
        VisibleTaskStatus::Settling => "Settling",
        VisibleTaskStatus::ReadyForReview => "Ready for review",
        VisibleTaskStatus::Idle => "Idle",
    }
}

fn validated_runtime_attachment(
    profile: &IsolatedDevProfile,
    host_runtime: Option<NativeHostClientRuntime>,
) -> (Option<NativeHostClientRuntime>, NativeHostState) {
    let Some(runtime) = host_runtime else {
        return (None, NativeHostState::Disconnected);
    };
    match runtime.validate_attachment(profile) {
        Ok(()) => {
            let endpoint = runtime.endpoint().to_string();
            (Some(runtime), NativeHostState::Connected { endpoint })
        }
        Err(error) => (
            None,
            NativeHostState::Error {
                message: bounded_host_error(error.to_string()),
            },
        ),
    }
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
        acquire_reaper_permit, authorize_full_host_quit, dispatch_pending_action,
        enqueue_pending_preference, ensure_isolated_host_config_base, isolated_dev_profile,
        publish_projection, reap_retained_children, reap_retained_workers, retain_child,
        retain_worker, retained_children, take_retained_action_outcomes, update_state_from_stage,
        wait_for_cancellation, AccessibilityTree, ClientId, CommandId, IsolatedDevProfile,
        NativeActionRecord, NativeHostActionFailure, NativeHostActionOutcome,
        NativeHostActionResult, NativeHostChildOwnership, NativeHostLaunchMode,
        NativeHostLaunchSpec, NativeHostProjection, NativeHostProjectionKind,
        NativeHostRuntimeEpochs, NativeHostRuntimePort, NativeHostWorkerCommand, NativeInteraction,
        NativePlatformAccessibilityBridge, NativeShell, NativeShellMode, NativeShutdownDeadline,
        OwnedChild, OwnedWorker, ReaperKind, TaskId, UpdateState, UpdaterStage,
        MAX_ACTION_LANE_RECORDS, MAX_ACTION_OUTCOME_PROJECTIONS, MAX_HOST_PROJECTIONS,
        MAX_PENDING_HOST_ACTIONS, MAX_PENDING_PREFERENCES, MAX_RETAINED_CHILDREN,
        MAX_RETAINED_WORKERS, MAX_RETRY_HOST_ACTIONS, PRODUCTION_HOST_PROFILE,
    };
    use gpui::AppContext;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::SyncSender;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    #[derive(Clone)]
    struct TestRuntime {
        shared: Arc<Mutex<TestRuntimeState>>,
    }

    struct TestRuntimeState {
        connected: bool,
        epochs: NativeHostRuntimeEpochs,
        admission: NativeHostActionResult,
        accepted: Vec<NativeActionRecord>,
        pending: VecDeque<NativeActionRecord>,
        projections: VecDeque<NativeHostProjection>,
        deferred: Option<NativeHostActionOutcome>,
    }

    static HEADLESS_SHELL_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    impl TestRuntime {
        fn new(
            connected: bool,
            admission: NativeHostActionResult,
        ) -> (Self, Arc<Mutex<TestRuntimeState>>) {
            let shared = Arc::new(Mutex::new(TestRuntimeState {
                connected,
                epochs: NativeHostRuntimeEpochs {
                    connection_epoch: 1,
                    resource_generation: 1,
                    runtime_generation: 1,
                },
                admission,
                accepted: Vec::new(),
                pending: VecDeque::new(),
                projections: VecDeque::new(),
                deferred: None,
            }));
            (
                Self {
                    shared: Arc::clone(&shared),
                },
                shared,
            )
        }
    }

    impl super::native_host_runtime_sealed::Sealed for TestRuntime {}

    impl NativeHostRuntimePort for TestRuntime {
        fn endpoint(&self) -> &str {
            "test://native-shell"
        }

        fn host_state(&self) -> super::NativeHostState {
            let state = self.shared.lock().expect("test runtime state");
            if state.connected {
                super::NativeHostState::Connected {
                    endpoint: self.endpoint().to_string(),
                }
            } else {
                super::NativeHostState::Disconnected
            }
        }

        fn epochs(&self) -> NativeHostRuntimeEpochs {
            self.shared.lock().expect("test runtime state").epochs
        }

        fn enqueue(&mut self, action: NativeActionRecord) -> NativeHostActionResult {
            let mut state = self.shared.lock().expect("test runtime state");
            if !state.connected {
                return NativeHostActionResult::Disconnected;
            }
            match state.admission {
                NativeHostActionResult::Queued => {
                    state.accepted.push(action);
                    NativeHostActionResult::Queued
                }
                result => result,
            }
        }

        fn drain_projection_messages(&mut self, max: usize) -> Vec<NativeHostProjection> {
            let mut state = self.shared.lock().expect("test runtime state");
            let count = max.min(state.projections.len());
            state.projections.drain(..count).collect()
        }

        fn take_deferred_action_outcome(&mut self) -> Option<NativeHostActionOutcome> {
            self.shared
                .lock()
                .expect("test runtime state")
                .deferred
                .take()
        }

        fn pending_front(&self) -> Option<&NativeActionRecord> {
            None
        }

        fn take_pending_front(&mut self) -> Option<NativeActionRecord> {
            self.shared
                .lock()
                .expect("test runtime state")
                .pending
                .pop_front()
        }

        fn pending_count(&self) -> usize {
            self.shared
                .lock()
                .expect("test runtime state")
                .pending
                .len()
        }

        fn dispatch_next_pending(&mut self) -> NativeHostActionResult {
            NativeHostActionResult::Queued
        }

        fn rebind_pending(&mut self, _epochs: NativeHostRuntimeEpochs, _navigation_epoch: u64) {}

        fn begin_shutdown(&mut self) {}

        fn action_lane_count(&self) -> usize {
            let state = self.shared.lock().expect("test runtime state");
            state
                .projections
                .iter()
                .filter(|projection| projection.action_outcome.is_some())
                .count()
                .saturating_add(state.pending.len())
                .saturating_add(usize::from(state.deferred.is_some()))
        }
    }

    struct PendingShutdownDropProbe {
        pending: VecDeque<NativeActionRecord>,
        command_tx: SyncSender<NativeHostWorkerCommand>,
        deadline: NativeShutdownDeadline,
        permit: Option<super::ReaperPermit>,
    }

    impl Drop for PendingShutdownDropProbe {
        fn drop(&mut self) {
            super::handoff_pending_actions_after_shutdown(
                &mut self.pending,
                &self.command_tx,
                self.deadline,
                self.permit.take().expect("drop probe action reaper permit"),
            );
        }
    }

    fn with_test_shell_in_app<R>(
        cx: &mut gpui::App,
        runtime: TestRuntime,
        action: impl FnOnce(&mut NativeShell) -> R,
    ) -> R {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");
        let entity = cx.new(|cx| {
            NativeShell::new_with_host_runtime_port(
                profile,
                Box::new(runtime),
                crate::ui::tokens::RuntimePreferencesSnapshot::default(),
                cx,
            )
        });
        let value = entity.update(cx, |shell, _cx| action(shell));
        drop(entity);
        value
    }

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
    fn blocked_worker_queue_keeps_the_exact_action_until_capacity_returns() {
        let (command_tx, _command_rx) = std::sync::mpsc::sync_channel(MAX_PENDING_HOST_ACTIONS);
        let mut interaction = NativeInteraction::new(Some(TaskId::new()));
        let action = interaction
            .action(crate::ui::components::ActionRequest::HostStatus)
            .expect("host status action");
        for _ in 0..MAX_PENDING_HOST_ACTIONS {
            command_tx
                .try_send(NativeHostWorkerCommand::Execute(action.clone()))
                .expect("fill worker queue");
        }
        let expected_id = action.id;
        let expected_task = action.task_id;
        let expected_command = format!("{:?}", action.command);
        let mut pending = VecDeque::from([action]);

        assert_eq!(
            dispatch_pending_action(&mut pending, &command_tx, None),
            NativeHostActionResult::QueueFull
        );
        let retained = pending.pop_front().expect("full worker retains action");
        assert_eq!(retained.id, expected_id);
        assert_eq!(retained.task_id, expected_task);
        assert_eq!(format!("{:?}", retained.command), expected_command);
    }

    #[test]
    fn disconnected_worker_queue_keeps_the_exact_action_for_typed_failure() {
        let (command_tx, command_rx) = std::sync::mpsc::sync_channel(2);
        drop(command_rx);
        let mut interaction = NativeInteraction::new(Some(TaskId::new()));
        let action = interaction
            .action(crate::ui::components::ActionRequest::HostStatus)
            .expect("host status action");
        let expected_id = action.id;
        let expected_task = action.task_id;
        let expected_command = format!("{:?}", action.command);
        let mut pending = VecDeque::from([action]);

        assert_eq!(
            dispatch_pending_action(&mut pending, &command_tx, None),
            NativeHostActionResult::Disconnected
        );
        let retained = pending
            .pop_front()
            .expect("disconnected worker retains action");
        assert_eq!(retained.id, expected_id);
        assert_eq!(retained.task_id, expected_task);
        assert_eq!(format!("{:?}", retained.command), expected_command);
    }

    #[cfg(windows)]
    #[test]
    fn child_reaper_keeps_owned_child_when_wait_observation_errors() {
        reap_retained_children();
        let permit = acquire_reaper_permit(ReaperKind::Child).expect("child permit");
        let mut child = exited_child();
        child.wait().expect("observe child exit");
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        unsafe {
            CloseHandle(HANDLE(child.as_raw_handle() as _)).expect("close observed child handle");
        }
        retain_child(OwnedChild {
            child,
            _permit: permit,
        });

        reap_retained_children();
        let children = retained_children()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(children.len(), 1, "wait errors retain child ownership");
        drop(children);

        let mut children = retained_children()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let retained = children.pop().expect("test retained child");
        drop(children);
        drop(retained);
    }

    #[test]
    fn host_epoch_resync_invalidates_shell_terminal_capture_before_new_press() {
        let task_id = TaskId::new();
        let mut interaction = NativeInteraction::new(Some(task_id));
        let first = interaction.terminal_mouse_down(
            1,
            task_id,
            crate::ui::shell::PointerButton::Primary,
            Some(task_id),
        );
        assert!(first.capture.is_ok(), "initial terminal press");

        assert!(interaction.sync_host_epochs(NativeHostRuntimeEpochs {
            connection_epoch: 2,
            resource_generation: 2,
            runtime_generation: 2,
        }));
        let second = interaction.terminal_mouse_down(
            2,
            task_id,
            crate::ui::shell::PointerButton::Primary,
            Some(task_id),
        );
        assert!(
            second.capture.is_ok(),
            "resync must release old shell capture"
        );
    }

    #[test]
    fn reconnect_rebind_updates_navigation_epoch_for_retained_action() {
        let task_id = TaskId::new();
        let mut interaction = NativeInteraction::new(Some(task_id));
        let _ = interaction.navigation_mouse_down(task_id, &super::TaskList::empty());
        let mut action = interaction
            .action(crate::ui::components::ActionRequest::HostStatus)
            .expect("captured action");
        let before = action.navigation_epoch;

        assert!(interaction.sync_host_epochs(NativeHostRuntimeEpochs {
            connection_epoch: 2,
            resource_generation: 2,
            runtime_generation: 2,
        }));
        let after = interaction.action_epochs().navigation_epoch;
        assert_ne!(before, after, "resync must advance navigation fencing");
        action.rebind_transport_epochs(
            NativeHostRuntimeEpochs {
                connection_epoch: 2,
                resource_generation: 2,
                runtime_generation: 2,
            },
            after,
        );
        assert_eq!(action.navigation_epoch, after);
        assert!(
            interaction.accepts_action_record(&action),
            "a retained action must be rebound to the current navigation epoch"
        );
    }

    fn ui_queue_full_pointer_admission_returns_and_retains_exact_record(cx: &mut gpui::App) {
        let (runtime, _shared) = TestRuntime::new(true, NativeHostActionResult::QueueFull);
        let (returned, retained, failure) = with_test_shell_in_app(cx, runtime, |shell| {
            let returned = shell.dispatch_pointer_action_for_test(
                crate::ui::components::ActionRequest::HostStatus,
                17,
            );
            (
                returned,
                shell.pending_action_for_test().cloned(),
                shell.last_action_failure().cloned(),
            )
        });
        let returned = returned.expect("queue-full admission returns exact record");
        assert_eq!(retained, Some(returned.clone()));
        assert_eq!(
            failure,
            Some(NativeHostActionFailure::QueueFull {
                action_id: returned.id,
            })
        );
    }

    fn disconnected_admission_rebinds_transport_epochs_and_retries_same_identity(
        cx: &mut gpui::App,
    ) {
        let (runtime, shared) = TestRuntime::new(false, NativeHostActionResult::Queued);
        let (original, accepted, failure) = with_test_shell_in_app(cx, runtime, |shell| {
            let returned = shell
                .dispatch_action_for_test(crate::ui::components::ActionRequest::TaskCreate(
                    crate::client::action::TaskCreateArguments {
                        task_id: TaskId::new(),
                        environment_id: crate::domain::id::EnvironmentId::new(),
                        title: "durable admission".to_string(),
                        description: None,
                        project_id: crate::domain::id::ProjectId::new(),
                        workspace: crate::domain::task::WorkspaceRef::Main,
                    },
                ))
                .expect("disconnected admission returns exact record");
            let original = returned.clone();
            {
                let mut state = shared.lock().expect("test runtime state");
                state.connected = true;
                state.epochs = NativeHostRuntimeEpochs {
                    connection_epoch: 2,
                    resource_generation: 2,
                    runtime_generation: 2,
                };
            }
            shell.controller_tick_for_test(MAX_PENDING_HOST_ACTIONS);
            let not_yet_reconciled = shared.lock().expect("test runtime state").accepted.clone();
            assert!(
                not_yet_reconciled.is_empty(),
                "reconnect must not auto-resubmit a retained action"
            );
            shell.reconcile_pending_host_actions(MAX_PENDING_HOST_ACTIONS);
            let accepted = shared.lock().expect("test runtime state").accepted.clone();
            (original, accepted, shell.last_action_failure().cloned())
        });
        assert_eq!(accepted.len(), 1);
        let retried = &accepted[0];
        assert_eq!(retried.id, original.id);
        assert_eq!(retried.task_id, original.task_id);
        assert_eq!(
            retried.expected_task_revision,
            original.expected_task_revision
        );
        assert_eq!(retried.command, original.command);
        assert_eq!(retried.client_epoch, original.client_epoch);
        assert_eq!(retried.connection_epoch, 2);
        assert_eq!(retried.resource_generation, 2);
        assert_eq!(retried.runtime_generation, 2);
        assert_eq!(failure, None, "queue-pressure status clears after retry");
    }

    fn uncertain_execution_retains_and_retries_same_command_identity(cx: &mut gpui::App) {
        let (runtime, shared) = TestRuntime::new(true, NativeHostActionResult::Queued);
        let (original, retained, accepted, failure) =
            with_test_shell_in_app(cx, runtime, |shell| {
                let _ = shell.dispatch_action_for_test(
                    crate::ui::components::ActionRequest::TaskCreate(
                        crate::client::action::TaskCreateArguments {
                            task_id: TaskId::new(),
                            environment_id: crate::domain::id::EnvironmentId::new(),
                            title: "uncertain execution".to_string(),
                            description: None,
                            project_id: crate::domain::id::ProjectId::new(),
                            workspace: crate::domain::task::WorkspaceRef::Main,
                        },
                    ),
                );
                let original = shared
                    .lock()
                    .expect("test runtime state")
                    .accepted
                    .first()
                    .cloned()
                    .expect("accepted action");
                let epochs = shared.lock().expect("test runtime state").epochs;
                shared
                    .lock()
                    .expect("test runtime state")
                    .projections
                    .push_back(NativeHostProjection {
                        kind: NativeHostProjectionKind::Error,
                        client_model: None,
                        error: Some("transport outcome uncertain".to_string()),
                        epochs: Some(epochs),
                        action_outcome: Some(NativeHostActionOutcome::Uncertain {
                            action: original.clone(),
                            error: "transport outcome uncertain".to_string(),
                        }),
                    });
                shell.controller_tick_for_test(MAX_PENDING_HOST_ACTIONS);
                let retained = shell.pending_action_for_test().cloned();
                shell.controller_tick_for_test(MAX_PENDING_HOST_ACTIONS);
                assert_eq!(
                    shared.lock().expect("test runtime state").accepted.len(),
                    1,
                    "uncertain outcomes require explicit reconciliation"
                );
                shell.reconcile_pending_host_actions(MAX_PENDING_HOST_ACTIONS);
                let accepted = shared.lock().expect("test runtime state").accepted.clone();
                (
                    original,
                    retained,
                    accepted,
                    shell.last_action_failure().cloned(),
                )
            });
        assert_eq!(retained, Some(original.clone()));
        assert_eq!(accepted.len(), 2, "retry is dispatched once");
        assert_eq!(accepted[1].command, original.command);
        assert_eq!(accepted[1].id, original.id);
        assert_eq!(
            failure, None,
            "successful same-command retry clears uncertain failure"
        );
    }

    fn failed_execution_retains_and_retries_same_command_identity(cx: &mut gpui::App) {
        let (runtime, shared) = TestRuntime::new(true, NativeHostActionResult::Queued);
        let (original, retained, failure, recovered_failure, accepted) =
            with_test_shell_in_app(cx, runtime, |shell| {
                let _ = shell.dispatch_action_for_test(
                    crate::ui::components::ActionRequest::TaskCreate(
                        crate::client::action::TaskCreateArguments {
                            task_id: TaskId::new(),
                            environment_id: crate::domain::id::EnvironmentId::new(),
                            title: "failed execution".to_string(),
                            description: None,
                            project_id: crate::domain::id::ProjectId::new(),
                            workspace: crate::domain::task::WorkspaceRef::Main,
                        },
                    ),
                );
                let original = shared
                    .lock()
                    .expect("test runtime state")
                    .accepted
                    .first()
                    .cloned()
                    .expect("accepted action");
                let epochs = shared.lock().expect("test runtime state").epochs;
                shared
                    .lock()
                    .expect("test runtime state")
                    .projections
                    .push_back(NativeHostProjection {
                        kind: NativeHostProjectionKind::Error,
                        client_model: None,
                        error: Some("host rejected command".to_string()),
                        epochs: Some(epochs),
                        action_outcome: Some(NativeHostActionOutcome::Failed {
                            action: original.clone(),
                            error: "host rejected command".to_string(),
                        }),
                    });
                let _ = shell.drain_host_projections(MAX_PENDING_HOST_ACTIONS);
                let retained = shell.pending_action_for_test().cloned();
                let failure = shell.last_action_failure().cloned();
                shell.controller_tick_for_test(MAX_PENDING_HOST_ACTIONS);
                assert_eq!(
                    shared.lock().expect("test runtime state").accepted.len(),
                    1,
                    "failed outcomes require explicit reconciliation"
                );
                shell.reconcile_pending_host_actions(MAX_PENDING_HOST_ACTIONS);
                let recovered_failure = shell.last_action_failure().cloned();
                let accepted = shared.lock().expect("test runtime state").accepted.clone();
                (original, retained, failure, recovered_failure, accepted)
            });
        assert_eq!(retained, Some(original.clone()));
        assert_eq!(accepted.len(), 2, "failed action is retried once");
        assert_eq!(accepted[1].id, original.id);
        assert_eq!(accepted[1].command, original.command);
        assert!(matches!(
            failure,
            Some(NativeHostActionFailure::ExecutionFailed {
                command_id: Some(command_id),
                ..
            }) if Some(command_id) == super::native_command_id(&original.command)
        ));
        assert_eq!(
            recovered_failure, None,
            "successful retry clears failed state"
        );
    }

    fn newer_projection_epoch_is_adopted_after_the_drain_snapshot(cx: &mut gpui::App) {
        let (runtime, shared) = TestRuntime::new(true, NativeHostActionResult::Queued);
        let adopted = with_test_shell_in_app(cx, runtime, |shell| {
            shared
                .lock()
                .expect("test runtime state")
                .projections
                .push_back(
                    NativeHostProjection::kind(NativeHostProjectionKind::Live).at_epochs(
                        NativeHostRuntimeEpochs {
                            connection_epoch: 2,
                            resource_generation: 2,
                            runtime_generation: 2,
                        },
                    ),
                );
            shell.controller_tick_for_test(MAX_PENDING_HOST_ACTIONS);
            shell.interaction.host_runtime_epochs()
        });
        assert_eq!(
            adopted,
            NativeHostRuntimeEpochs {
                connection_epoch: 2,
                resource_generation: 2,
                runtime_generation: 2,
            },
            "a newer projection must not be dropped by the pre-drain snapshot"
        );
    }

    fn mixed_newer_projection_epoch_is_merged_not_dropped(cx: &mut gpui::App) {
        let (runtime, shared) = TestRuntime::new(true, NativeHostActionResult::Queued);
        let adopted = with_test_shell_in_app(cx, runtime, |shell| {
            shared
                .lock()
                .expect("test runtime state")
                .projections
                .push_back(
                    NativeHostProjection::kind(NativeHostProjectionKind::Live).at_epochs(
                        NativeHostRuntimeEpochs {
                            connection_epoch: 1,
                            resource_generation: 2,
                            runtime_generation: 1,
                        },
                    ),
                );
            shell.controller_tick_for_test(MAX_PENDING_HOST_ACTIONS);
            shell.interaction.host_runtime_epochs()
        });
        assert_eq!(
            adopted,
            NativeHostRuntimeEpochs {
                connection_epoch: 1,
                resource_generation: 2,
                runtime_generation: 1,
            },
            "a projection newer on one lane must merge instead of being dropped"
        );
    }

    fn stale_action_outcomes_are_epoch_fenced(cx: &mut gpui::App) {
        let (runtime, shared) = TestRuntime::new(true, NativeHostActionResult::Queued);
        let (receipt, failure, retained) = with_test_shell_in_app(cx, runtime, |shell| {
            let _ =
                shell.dispatch_action_for_test(crate::ui::components::ActionRequest::HostStatus);
            let action = shared
                .lock()
                .expect("test runtime state")
                .accepted
                .first()
                .cloned()
                .expect("accepted action");
            let old_epochs = shared.lock().expect("test runtime state").epochs;
            shared.lock().expect("test runtime state").epochs = NativeHostRuntimeEpochs {
                connection_epoch: 2,
                resource_generation: 2,
                runtime_generation: 2,
            };
            shared
                .lock()
                .expect("test runtime state")
                .projections
                .push_back(NativeHostProjection {
                    kind: NativeHostProjectionKind::Live,
                    client_model: None,
                    error: None,
                    epochs: Some(old_epochs),
                    action_outcome: Some(NativeHostActionOutcome::Accepted {
                        action,
                        receipt: crate::domain::command::CommandReceipt::Accepted {
                            command_id: crate::domain::id::CommandId::new(),
                            operation_id: crate::domain::id::OperationId::new(),
                            task_revision: None,
                            event_ids: Vec::new(),
                            prompt_mutation: None,
                        },
                    }),
                });
            shell.controller_tick_for_test(MAX_PENDING_HOST_ACTIONS);
            (
                shell.last_action_receipt().cloned(),
                shell.last_action_failure().cloned(),
                shell.pending_action_for_test().cloned(),
            )
        });
        assert_eq!(receipt, None, "stale receipts must not mutate shell state");
        assert!(matches!(
            failure,
            Some(NativeHostActionFailure::Stale { .. })
        ));
        assert!(
            retained.is_some(),
            "stale outcome identity remains retained"
        );
        let _ = take_retained_action_outcomes(MAX_ACTION_LANE_RECORDS);
    }

    fn action_outcome_retention_pressure_keeps_exact_overflow_record(cx: &mut gpui::App) {
        let (runtime, _shared) = TestRuntime::new(true, NativeHostActionResult::Queued);
        let (pending_len, overflow, failure) = with_test_shell_in_app(cx, runtime, |shell| {
            for _ in 0..MAX_RETRY_HOST_ACTIONS {
                let action = shell
                    .interaction
                    .action(crate::ui::components::ActionRequest::HostStatus)
                    .expect("retry-lane action identity");
                assert!(shell.retain_pending_host_action(action));
            }
            let overflow = shell
                .interaction
                .action(crate::ui::components::ActionRequest::HostStatus)
                .expect("outcome overflow identity");
            shell.apply_action_outcome(NativeHostActionOutcome::Uncertain {
                action: overflow.clone(),
                error: "outcome pressure".to_string(),
            });
            (
                shell.pending_host_actions.len(),
                shell.retained_action_overflow.clone(),
                (overflow, shell.last_action_failure().cloned()),
            )
        });
        assert_eq!(pending_len, MAX_RETRY_HOST_ACTIONS);
        assert_eq!(overflow, Some(failure.0));
        assert!(matches!(
            failure.1,
            Some(NativeHostActionFailure::ExecutionUncertain { .. })
        ));
    }

    fn native_shell_drop_retains_pending_overflow_and_deferred_as_uncertain(cx: &mut gpui::App) {
        let _ = take_retained_action_outcomes(MAX_ACTION_LANE_RECORDS);
        let (runtime, shared) = TestRuntime::new(true, NativeHostActionResult::Queued);
        let expected = with_test_shell_in_app(cx, runtime, |shell| {
            let pending = shell
                .interaction
                .action(crate::ui::components::ActionRequest::HostStatus)
                .expect("pending action");
            let overflow = shell
                .interaction
                .action(crate::ui::components::ActionRequest::HostStatus)
                .expect("overflow action");
            let deferred = shell
                .interaction
                .action(crate::ui::components::ActionRequest::HostStatus)
                .expect("deferred action");
            let runtime_pending = shell
                .interaction
                .action(crate::ui::components::ActionRequest::HostStatus)
                .expect("runtime pending action");
            shell.pending_host_actions.push_back(pending.clone());
            shell.retained_action_overflow = Some(overflow.clone());
            let mut state = shared.lock().expect("test runtime state");
            state.pending.push_back(runtime_pending.clone());
            state.deferred = Some(NativeHostActionOutcome::Failed {
                action: deferred.clone(),
                error: "deferred failure".to_string(),
            });
            vec![pending, overflow, runtime_pending, deferred]
        });

        let outcomes = take_retained_action_outcomes(MAX_ACTION_LANE_RECORDS);
        let actual = outcomes
            .iter()
            .map(|outcome| match outcome {
                NativeHostActionOutcome::Uncertain { action, .. } => action.clone(),
                other => panic!("shell drop must retain uncertainty, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn shutdown_handoff_retains_disconnected_pending_actions_as_uncertain() {
        let _ = take_retained_action_outcomes(MAX_ACTION_LANE_RECORDS);
        let (command_tx, command_rx) = std::sync::mpsc::sync_channel(1);
        drop(command_rx);
        let mut interaction = NativeInteraction::new(Some(TaskId::new()));
        let first = interaction
            .action(crate::ui::components::ActionRequest::HostStatus)
            .expect("first pending action");
        let second = interaction
            .action(crate::ui::components::ActionRequest::HostStatus)
            .expect("second pending action");
        let expected = vec![first.clone(), second.clone()];
        let permit = acquire_reaper_permit(ReaperKind::ActionBatch).expect("action batch permit");
        {
            let _probe = PendingShutdownDropProbe {
                pending: VecDeque::from([first, second]),
                command_tx,
                deadline: NativeShutdownDeadline::from_now(Duration::from_secs(1)),
                permit: Some(permit),
            };
        }
        let outcomes = take_retained_action_outcomes(MAX_ACTION_LANE_RECORDS);
        let actual = outcomes
            .iter()
            .map(|outcome| match outcome {
                NativeHostActionOutcome::Uncertain { action, .. } => action.clone(),
                other => panic!("unexpected shutdown outcome: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn shutdown_handoff_retains_deadline_expired_pending_actions_without_resend() {
        let _ = take_retained_action_outcomes(MAX_ACTION_LANE_RECORDS);
        let (command_tx, command_rx) = std::sync::mpsc::sync_channel(2);
        let mut interaction = NativeInteraction::new(Some(TaskId::new()));
        let filler = interaction
            .action(crate::ui::components::ActionRequest::HostStatus)
            .expect("filler action");
        let pending_action = interaction
            .action(crate::ui::components::ActionRequest::HostStatus)
            .expect("pending action");
        command_tx
            .try_send(NativeHostWorkerCommand::Execute(filler))
            .expect("fill command channel");
        let expected_action = pending_action.clone();
        let permit = acquire_reaper_permit(ReaperKind::ActionBatch).expect("action batch permit");
        {
            let _probe = PendingShutdownDropProbe {
                pending: VecDeque::from([pending_action]),
                command_tx: command_tx.clone(),
                deadline: NativeShutdownDeadline::from_now(Duration::from_secs(1)),
                permit: Some(permit),
            };
        }
        let outcomes = take_retained_action_outcomes(MAX_ACTION_LANE_RECORDS);
        assert!(matches!(
            outcomes.as_slice(),
            [NativeHostActionOutcome::Uncertain { action, .. }] if action == &expected_action
        ));
        assert!(matches!(
            command_rx.try_recv(),
            Ok(NativeHostWorkerCommand::Execute(_))
        ));
        assert!(
            command_rx.try_recv().is_err(),
            "pending action must not be enqueued after shutdown cancellation"
        );
    }

    #[test]
    fn action_admission_and_outcome_durability() {
        // GPUI's Windows headless message loop is process-global. Exercise
        // every injected-runtime scenario in one app lifetime so teardown
        // cannot race the next headless application instance.
        let _test_guard = HEADLESS_SHELL_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("headless shell test lock");
        let completed = std::rc::Rc::new(std::cell::RefCell::new(false));
        let completed_for_app = std::rc::Rc::clone(&completed);
        gpui::Application::headless().run(move |cx| {
            crate::ui::init(cx);
            ui_queue_full_pointer_admission_returns_and_retains_exact_record(cx);
            disconnected_admission_rebinds_transport_epochs_and_retries_same_identity(cx);
            uncertain_execution_retains_and_retries_same_command_identity(cx);
            failed_execution_retains_and_retries_same_command_identity(cx);
            newer_projection_epoch_is_adopted_after_the_drain_snapshot(cx);
            mixed_newer_projection_epoch_is_merged_not_dropped(cx);
            stale_action_outcomes_are_epoch_fenced(cx);
            action_outcome_retention_pressure_keeps_exact_overflow_record(cx);
            native_shell_drop_retains_pending_overflow_and_deferred_as_uncertain(cx);
            *completed_for_app.borrow_mut() = true;
            cx.quit();
        });
        assert!(*completed.borrow(), "action durability scenarios completed");
    }

    #[test]
    fn action_outcome_projection_survives_a_full_normal_projection_lane() {
        let projections = Arc::new(Mutex::new(VecDeque::new()));
        for _ in 0..MAX_HOST_PROJECTIONS {
            publish_projection(
                &projections,
                NativeHostProjection::kind(NativeHostProjectionKind::Replay),
            );
        }
        let mut interaction = NativeInteraction::new(Some(TaskId::new()));
        let action = interaction
            .action(crate::ui::components::ActionRequest::HostStatus)
            .expect("action outcome identity");
        assert!(publish_projection(
            &projections,
            NativeHostProjection {
                kind: NativeHostProjectionKind::Error,
                client_model: None,
                error: Some("outcome uncertain".to_string()),
                epochs: None,
                action_outcome: Some(NativeHostActionOutcome::Uncertain {
                    action: action.clone(),
                    error: "outcome uncertain".to_string(),
                }),
            },
        ));

        let queue = projections.lock().expect("projection queue");
        assert!(queue.iter().any(|projection| {
            matches!(
                projection.action_outcome.as_ref(),
                Some(NativeHostActionOutcome::Uncertain { action: retained, .. })
                    if retained.id == action.id
            )
        }));
        assert_eq!(queue.len(), MAX_HOST_PROJECTIONS + 1);
    }

    #[test]
    fn action_outcome_backpressure_is_typed_and_never_silent() {
        let projections = Arc::new(Mutex::new(VecDeque::new()));
        for _ in 0..MAX_ACTION_OUTCOME_PROJECTIONS {
            let mut interaction = NativeInteraction::new(Some(TaskId::new()));
            let action = interaction
                .action(crate::ui::components::ActionRequest::HostStatus)
                .expect("action outcome identity");
            assert!(publish_projection(
                &projections,
                NativeHostProjection {
                    kind: NativeHostProjectionKind::Error,
                    client_model: None,
                    error: Some("outcome retained".to_string()),
                    epochs: None,
                    action_outcome: Some(NativeHostActionOutcome::Uncertain {
                        action,
                        error: "outcome retained".to_string(),
                    }),
                },
            ));
        }
        let mut interaction = NativeInteraction::new(Some(TaskId::new()));
        let overflow = interaction
            .action(crate::ui::components::ActionRequest::HostStatus)
            .expect("overflow action identity");
        assert!(
            !publish_projection(
                &projections,
                NativeHostProjection {
                    kind: NativeHostProjectionKind::Error,
                    client_model: None,
                    error: Some("outcome pressure".to_string()),
                    epochs: None,
                    action_outcome: Some(NativeHostActionOutcome::Uncertain {
                        action: overflow,
                        error: "outcome pressure".to_string(),
                    }),
                },
            ),
            "outcome saturation must be observable instead of silently dropping a record"
        );
    }

    #[test]
    fn worker_publication_uses_a_bounded_second_overflow_lane() {
        let projections = Arc::new(Mutex::new(VecDeque::new()));
        for _ in 0..MAX_ACTION_OUTCOME_PROJECTIONS {
            let mut interaction = NativeInteraction::new(Some(TaskId::new()));
            let action = interaction
                .action(crate::ui::components::ActionRequest::HostStatus)
                .expect("lane-filling action");
            assert!(publish_projection(
                &projections,
                NativeHostProjection {
                    kind: NativeHostProjectionKind::Error,
                    client_model: None,
                    error: Some("lane full".to_string()),
                    epochs: None,
                    action_outcome: Some(NativeHostActionOutcome::Uncertain {
                        action,
                        error: "lane full".to_string(),
                    }),
                },
            ));
        }
        let mut deferred_action = NativeInteraction::new(Some(TaskId::new()));
        let deferred = Arc::new(Mutex::new(Some(NativeHostActionOutcome::Uncertain {
            action: deferred_action
                .action(crate::ui::components::ActionRequest::HostStatus)
                .expect("deferred action"),
            error: "deferred".to_string(),
        })));
        let worker_overflow = Arc::new(Mutex::new(VecDeque::new()));
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut overflow_action = NativeInteraction::new(Some(TaskId::new()));
        let overflow_action = overflow_action
            .action(crate::ui::components::ActionRequest::HostStatus)
            .expect("second overflow action");

        super::publish_worker_projection(
            &projections,
            &deferred,
            &worker_overflow,
            NativeHostProjection {
                kind: NativeHostProjectionKind::Error,
                client_model: None,
                error: Some("worker overflow".to_string()),
                epochs: None,
                action_outcome: Some(NativeHostActionOutcome::Uncertain {
                    action: overflow_action,
                    error: "worker overflow".to_string(),
                }),
            },
            &cancellation,
        );

        let overflow = worker_overflow.lock().expect("worker overflow lane");
        assert_eq!(overflow.len(), 1);
        assert!(overflow.len() <= MAX_ACTION_LANE_RECORDS);
    }

    #[test]
    fn product_client_build_never_uses_devmanager_next_prefix() {
        let workspace = tempfile::tempdir().expect("workspace");
        let profile = isolated_dev_profile(workspace.path()).expect("isolated profile");
        assert!(profile
            .host_client_config()
            .client_build
            .starts_with("devmanager/"));
        assert!(!profile
            .host_client_config()
            .client_build
            .contains("devmanager-next"));
    }

    #[test]
    fn isolated_launch_spec_parent_binds_and_production_omits_parent_pid() {
        let workspace = tempfile::tempdir().expect("workspace");
        let isolated = isolated_dev_profile(workspace.path()).expect("isolated profile");
        let isolated_spec = NativeHostLaunchSpec {
            executable: PathBuf::from("devmanager-host.exe"),
            mode: NativeHostLaunchMode::Isolated {
                profile: isolated.named_profile().to_string(),
                instance_label: "Native Debug".to_string(),
                parent_pid: 42,
                config_base: isolated.host_config_base().to_path_buf(),
            },
        };
        let isolated_args = isolated_spec.arguments();
        assert!(isolated_args.iter().any(|arg| arg == "--parent-pid"));
        assert!(isolated_args.iter().any(|arg| arg == "--config-base"));
        assert_eq!(isolated.mode(), NativeShellMode::IsolatedDebug);

        let production = IsolatedDevProfile {
            workspace_root: isolated.root().to_path_buf(),
            root: isolated.root().to_path_buf(),
            named_profile: PRODUCTION_HOST_PROFILE.to_string(),
            mode: NativeShellMode::Production,
        };
        let production_spec = NativeHostLaunchSpec {
            executable: PathBuf::from("devmanager-host.exe"),
            mode: NativeHostLaunchMode::Production,
        };
        let production_args = production_spec.arguments();
        assert_eq!(production_args, vec!["--foreground".to_string()]);
        assert!(!production_args.iter().any(|arg| arg == "--parent-pid"));
        assert_eq!(
            production.child_ownership(),
            NativeHostChildOwnership::DetachOnClientClose
        );
        assert_eq!(
            isolated.child_ownership(),
            NativeHostChildOwnership::TerminateWithClient
        );
    }

    #[test]
    fn native_host_process_detach_ownership_does_not_require_child_kill_path() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/ui/native_shell.rs"
        ));
        assert!(source.contains("DetachOnClientClose"));
        assert!(source.contains("try_attach_existing_host"));
        assert!(source.contains("PRODUCTION_HOST_PROFILE"));
        assert!(!source.contains("\"devmanager-next/"));
        assert!(!source.contains("Native Next"));
    }

    #[test]
    fn native_profile_identity_is_workspace_bound_not_global() {
        let first = tempfile::tempdir().expect("first workspace");
        let second = tempfile::tempdir().expect("second workspace");
        let first_profile = isolated_dev_profile(first.path()).expect("first profile");
        let second_profile = isolated_dev_profile(second.path()).expect("second profile");
        assert_ne!(
            first_profile.named_profile(),
            second_profile.named_profile(),
            "IPC profile identity must include the workspace binding"
        );
        assert_ne!(first_profile.named_profile(), "native-next-dev");
        assert_eq!(
            first_profile.host_client_config().named_profile,
            first_profile.named_profile()
        );
        assert_ne!(
            first_profile.host_connection().endpoint(),
            second_profile.host_connection().endpoint()
        );
    }

    #[test]
    fn action_lane_cap_aggregates_channel_pending_and_shell_retained() {
        assert_eq!(
            super::action_lane_total(MAX_ACTION_LANE_RECORDS - 2, 1, 1),
            MAX_ACTION_LANE_RECORDS
        );
        assert!(
            super::action_lane_total(MAX_ACTION_LANE_RECORDS - 1, 1, 1) > MAX_ACTION_LANE_RECORDS,
            "the admission cap must see every action lane, not only shell retries"
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_host_executable_rejects_symlink_redirection() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let real = workspace.path().join("devmanager-host-real");
        let redirected = workspace.path().join("devmanager-host");
        std::fs::write(&real, b"host").expect("real host");
        symlink(&real, &redirected).expect("redirected host symlink");
        assert!(
            super::validate_native_host_executable(&redirected).is_err(),
            "a sibling executable must not follow symlink redirection"
        );
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

    #[test]
    fn spawned_host_environment_sanitizer_clears_devmanager_identity_overrides() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/ui/native_shell.rs"
        ));
        assert!(source.contains("fn sanitize_spawned_host_environment"));
        assert!(source.contains("sanitize_spawned_host_environment(&mut command)"));
        for key in [
            "DEVMANAGER_PROFILE",
            "DEVMANAGER_INSTANCE_LABEL",
            "DEVMANAGER_RUNTIME_KIND",
            "DEVMANAGER_CONFIG_DIR",
            "DEVMANAGER_APP_IDENTITY",
        ] {
            assert!(
                source.contains(&format!("\"{key}\"")),
                "sanitizer must clear {key}"
            );
        }
    }

    #[test]
    fn authorize_full_host_quit_uses_inspect_facts_not_always_false_confirmable() {
        use crate::domain::host::{
            HostQuitInspection, HostQuitResourceBlocker, HostQuitWorktreeInspection,
        };
        use crate::domain::id::ResourceId;
        use crate::domain::resource::{OwnerKind, ResourceKind, ResourceLifecycle};

        let clean = HostQuitInspection {
            inspection_id: 7,
            agents: Vec::new(),
            resources: Vec::new(),
            worktrees: HostQuitWorktreeInspection::NotInspected,
            confirmable: false,
        };
        assert!(
            authorize_full_host_quit(&clean, false).is_err(),
            "NotInspected without authorization must fail closed"
        );
        assert!(
            authorize_full_host_quit(&clean, true).is_ok(),
            "explicit allow_uninspected_worktrees authorizes NotInspected when no blockers"
        );

        let mut confirmable = clean.clone();
        confirmable.confirmable = true;
        assert!(authorize_full_host_quit(&confirmable, false).is_ok());

        let blocked = HostQuitInspection {
            inspection_id: 8,
            agents: Vec::new(),
            resources: vec![HostQuitResourceBlocker {
                resource_id: ResourceId::from_bytes([
                    0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0xb1,
                ])
                .expect("resource"),
                task_id: None,
                task_title: None,
                owner_kind: OwnerKind::Host,
                resource_kind: ResourceKind::Terminal,
                lifecycle: ResourceLifecycle::Active,
                runtime_generation: 0,
            }],
            worktrees: HostQuitWorktreeInspection::NotInspected,
            confirmable: false,
        };
        assert!(
            authorize_full_host_quit(&blocked, true).is_err(),
            "resource blockers must fail closed even with uninspected authorization"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_host_two_clients_attach_detach_and_inspect_confirm_full_quit() {
        use crate::domain::command::{
            Command, CommandEnvelope, CommandReceipt, ConfirmHostQuitIntent,
        };
        use crate::domain::id::RequestId;
        use crate::domain::query::{Query, QueryEnvelope, QueryOutcome, QueryResult};
        use crate::host::{
            ConnectionOutputHandle, ConnectionOutputId, HostRequestExecutor, OutputInspection,
        };
        use crate::kernel::CommandBus;
        use crate::protocol::{
            Capability, CapabilitySet, ClientRequest, DetachAck, DetachRequest, FrameLimits,
            NegotiatedParameters, ProtocolVersion, ServerMessage,
        };

        let dir = tempfile::tempdir().expect("tempdir");
        // Isolated temp DB — never production profile / installed app root.
        let bus = CommandBus::open(&dir.path().join("phase11-entry.db")).expect("bus");
        let (requests, executor) = HostRequestExecutor::start(bus);

        let id_a = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xa1,
        ]);
        let id_b = uuid::Uuid::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xa2,
        ]);
        let (out_a, mut ports_a) = ConnectionOutputHandle::with_connection_id(id_a, 2, 4, 1);
        let (out_b, _ports_b) = ConnectionOutputHandle::with_connection_id(id_b, 2, 4, 1);
        let shutdown_a = out_a.subscribe_shutdown();
        let reg_a = requests
            .register_output(out_a.clone())
            .await
            .expect("register client A");
        let reg_b = requests
            .register_output(out_b.clone())
            .await
            .expect("register client B");
        assert_eq!(reg_a.id().as_uuid(), id_a);
        assert_eq!(reg_b.id().as_uuid(), id_b);

        let client = ClientId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xa3,
        ])
        .expect("client");
        let negotiated = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: client,
            capabilities: CapabilitySet::from_capabilities([
                Capability::ExplicitDetach,
                Capability::HostShutdown,
                Capability::PagedSnapshots,
                Capability::EventReplay,
            ]),
            limits: FrameLimits::v1_default(),
        };
        let handle_a = requests.with_output(reg_a.id());
        let handle_b = requests.with_output(reg_b.id());

        let detach_request_id = RequestId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xa4,
        ])
        .expect("detach request");
        let ack_message = handle_a
            .execute(
                negotiated,
                ClientRequest::Detach(DetachRequest {
                    request_id: detach_request_id,
                    client_id: client,
                    connection_id: id_a,
                }),
            )
            .await
            .expect("acknowledged detach");
        assert_eq!(
            ack_message,
            ServerMessage::Detached(DetachAck {
                request_id: detach_request_id,
                connection_id: id_a,
            })
        );
        assert_eq!(
            requests
                .inspect_output(ConnectionOutputId::from_uuid(id_a))
                .await
                .expect("inspect A after detach"),
            OutputInspection {
                registered: false,
                live_bound: false,
            }
        );
        assert!(
            requests
                .inspect_output(ConnectionOutputId::from_uuid(id_b))
                .await
                .expect("inspect B after A detach")
                .registered,
            "second client must remain attached"
        );
        out_a
            .try_enqueue_critical_shutdown_after_write(ack_message.clone())
            .expect("admit detach ack");
        let outbound = ports_a
            .try_recv_prioritized()
            .expect("detach ack on critical lane");
        assert_eq!(outbound.message(), &ack_message);
        outbound.after_successful_write();
        assert!(*shutdown_a.borrow());

        let inspect = handle_b
            .execute(
                negotiated,
                ClientRequest::Query(QueryEnvelope {
                    request_id: RequestId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xa5,
                    ])
                    .expect("inspect request"),
                    client_id: client,
                    task_id: None,
                    query: Query::InspectHostQuit,
                }),
            )
            .await
            .expect("inspect");
        let ServerMessage::QueryReply(reply) = inspect else {
            panic!("expected inspect query reply, got {inspect:?}");
        };
        let QueryOutcome::Ok(QueryResult::HostQuitInspection { inspection }) = reply.outcome else {
            panic!("expected HostQuitInspection, got {:?}", reply.outcome);
        };
        assert!(
            !inspection.confirmable,
            "current host slice reports confirmable=false while worktrees are NotInspected"
        );
        authorize_full_host_quit(&inspection, true).expect("authorized uninspected path");
        let confirmed = handle_b
            .execute(
                negotiated,
                ClientRequest::Command(CommandEnvelope {
                    command_id: CommandId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0xa6,
                    ])
                    .expect("confirm command"),
                    client_id: client,
                    task_id: None,
                    issued_at_ms: 1_725_000_000_500,
                    expected_task_revision: None,
                    command: Command::ConfirmHostQuit(ConfirmHostQuitIntent {
                        inspection_id: inspection.inspection_id,
                        allow_uninspected_worktrees: true,
                    }),
                }),
            )
            .await
            .expect("confirm");
        assert!(
            matches!(
                confirmed,
                ServerMessage::CommandReceipt(CommandReceipt::Accepted { .. })
            ),
            "inspect→confirm full quit must Accept, got {confirmed:?}"
        );
        drop(executor);
    }

    #[test]
    fn attach_timeout_is_preserved_and_never_treated_as_missing_pipe() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/ui/native_shell.rs"
        ));
        assert!(
            source.contains("return Err(IpcError::Timeout)"),
            "slow connect/bootstrap must map to Timeout"
        );
        let attach_loop = source
            .split("match try_attach_existing_host")
            .nth(1)
            .unwrap_or_default();
        assert!(
            attach_loop.contains("Err(IpcError::Unavailable) => break"),
            "only Unavailable may fall through to spawn"
        );
        assert!(
            attach_loop.contains("Err(IpcError::Timeout)")
                && attach_loop.contains("continue")
                && !attach_loop
                    .lines()
                    .take(40)
                    .any(|line| line.contains("Err(IpcError::Busy) | Err(IpcError::Timeout)")),
            "Timeout must retry separately from Busy and must not spawn"
        );
    }

    #[test]
    fn service_control_accepts_current_public_action_epoch_and_rejects_stale() {
        use crate::client::action::ServiceControlArguments;
        use crate::domain::command::ServiceControlAction;
        use crate::services::model::ServiceId;
        use crate::ui::components::ActionRequest;

        let mut interaction = NativeInteraction::new(None);
        interaction.sync_host_epochs(NativeHostRuntimeEpochs {
            connection_epoch: 2,
            resource_generation: 3,
            runtime_generation: 1,
        });
        let _ = interaction
            .action(ActionRequest::HostStatus)
            .expect("prime action epoch");
        let public = interaction.action_epochs();
        assert!(public.action_epoch > 0);

        let accepted = interaction
            .action(ActionRequest::ServiceControl {
                action: ServiceControlAction::Start,
                arguments: ServiceControlArguments {
                    service_id: ServiceId::new("web").expect("service id"),
                    resource_generation: public.resource_generation,
                    connection_epoch: public.connection_epoch,
                    action_epoch: public.action_epoch,
                },
            })
            .expect("current public action epoch must enqueue");
        assert!(matches!(
            accepted.command,
            super::NativeHostCommand::ServiceControl { .. }
        ));

        let stale_epoch = public.action_epoch;
        let rejected = interaction.action(ActionRequest::ServiceControl {
            action: ServiceControlAction::Stop,
            arguments: ServiceControlArguments {
                service_id: ServiceId::new("web").expect("service id"),
                resource_generation: public.resource_generation,
                connection_epoch: public.connection_epoch,
                action_epoch: stale_epoch,
            },
        });
        assert!(
            rejected.is_none(),
            "changed public action epoch must reject ServiceControl"
        );
    }

    #[test]
    fn visible_query_actions_never_map_to_hold() {
        use crate::ui::components::ActionRequest;

        let selected = TaskId::new();
        let mut interaction = NativeInteraction::new(Some(selected));
        let cases = [
            interaction
                .action(ActionRequest::HostActions)
                .expect("host.actions"),
            interaction
                .action(ActionRequest::HostStatus)
                .expect("host.status"),
            interaction
                .action(ActionRequest::TaskList)
                .expect("task.list"),
            interaction
                .action(ActionRequest::TaskShow { task_id: selected })
                .expect("task.show"),
        ];
        for record in cases {
            assert!(
                !matches!(record.command, super::NativeHostCommand::Hold { .. }),
                "{} must not become Hold",
                record.id
            );
        }
        assert!(matches!(
            interaction
                .action(ActionRequest::HostActions)
                .expect("host.actions again")
                .command,
            super::NativeHostCommand::HostActionsQuery { .. }
        ));
        assert!(matches!(
            interaction
                .action(ActionRequest::HostStatus)
                .expect("host.status again")
                .command,
            super::NativeHostCommand::HostStatusQuery { .. }
        ));
        assert!(matches!(
            interaction
                .action(ActionRequest::TaskList)
                .expect("task.list again")
                .command,
            super::NativeHostCommand::TaskListQuery { .. }
        ));
        assert!(matches!(
            interaction
                .action(ActionRequest::TaskShow { task_id: selected })
                .expect("task.show again")
                .command,
            super::NativeHostCommand::TaskShowQuery { .. }
        ));
        assert!(matches!(
            interaction
                .action(ActionRequest::TaskCockpit {
                    task_id: selected,
                    query: crate::domain::TaskCockpitQuery::GitStatus,
                })
                .expect("git.status")
                .command,
            super::NativeHostCommand::TaskCockpitQuery { .. }
        ));
        assert!(matches!(
            interaction
                .action(ActionRequest::PromptLibrary {
                    query: crate::prompts::projection::PromptLibraryQuery::MetadataPage {
                        namespace: crate::prompts::projection::PromptNamespace::Personal,
                        cursor: None,
                        expected_revision: None,
                    },
                })
                .expect("prompt.library")
                .command,
            super::NativeHostCommand::PromptLibraryQuery { .. }
        ));
        assert!(matches!(
            interaction
                .action(ActionRequest::Updater(
                    crate::client::action::UpdaterAction::Check
                ))
                .expect("updater.check")
                .command,
            super::NativeHostCommand::Updater { .. }
        ));
        assert_eq!(
            update_state_from_stage(UpdaterStage::ReadyToInstall),
            UpdateState::ReadyToInstall
        );
    }

    #[test]
    fn action_outcome_survives_client_model_epoch_advancement() {
        use crate::ui::components::ActionRequest;

        let selected = TaskId::new();
        let mut interaction = NativeInteraction::new(Some(selected));
        interaction.sync_host_epochs(NativeHostRuntimeEpochs {
            connection_epoch: 1,
            resource_generation: 1,
            runtime_generation: 1,
        });
        let record = interaction
            .action(ActionRequest::HostStatus)
            .expect("capture host status");
        assert!(interaction.accepts_action_record(&record));
        interaction.set_client_model(None);
        assert!(
            !interaction.accepts_action_record(&record),
            "exact client_epoch still required for enqueue/admission"
        );
        assert!(
            interaction.accepts_action_outcome_record(&record),
            "queued ClientModel projection must not permanently stale a valid outcome"
        );
    }

    fn terminal_bound_client_model() -> (crate::client::ClientModel, TaskId) {
        use crate::client::ClientModelBuilder;
        use crate::domain::{
            agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle},
            id::{AgentSessionId, EnvironmentId, ProjectId, ResourceId, SnapshotId},
            resource::{OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe},
            snapshot::{SnapshotItem, SnapshotPage, SnapshotSection, TaskSnapshotItem},
            task::{
                ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
                TaskFacts, TaskLifecycle, WorkspaceRef,
            },
        };

        let uuid = |tail: u8| {
            [
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, tail,
            ]
        };
        let task_id = TaskId::from_bytes(uuid(0xa1)).expect("task");
        let agent_id = AgentSessionId::from_bytes(uuid(0xa2)).expect("agent");
        let resource_id = ResourceId::from_bytes(uuid(0xa3)).expect("resource");
        let snap = SnapshotId::from_bytes(uuid(0x10)).expect("snapshot");
        let page = |section, items| SnapshotPage {
            snapshot_id: snap,
            through_sequence: 1,
            section,
            after_item: None,
            items,
            encoded_bytes: 1,
            next_cursor: None,
        };
        let mut builder = ClientModelBuilder::new();
        builder
            .ingest_page(page(
                SnapshotSection::Tasks,
                vec![SnapshotItem::Task(TaskSnapshotItem {
                    task: TaskFacts {
                        id: task_id,
                        environment_id: EnvironmentId::from_bytes(uuid(0x01)).expect("env"),
                        title: "Bound terminal task".into(),
                        description: None,
                        project_id: ProjectId::from_bytes(uuid(0x02)).expect("project"),
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        lifecycle: TaskLifecycle::Open,
                        action_epoch: 3,
                        revision: 4,
                        created_at_ms: 1,
                    },
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                    primary_agent_id: Some(agent_id),
                })],
            ))
            .expect("tasks");
        builder
            .ingest_page(page(
                SnapshotSection::AgentSessions,
                vec![SnapshotItem::AgentSession(AgentSessionFacts {
                    id: agent_id,
                    task_id,
                    role: AgentRole::Primary,
                    provider_kind: crate::providers::ProviderKind::ClaudeCode,
                    provider_session_id: None,
                    lifecycle: AgentSessionLifecycle::Open,
                    runtime_generation: 1,
                    revision: 0,
                })],
            ))
            .expect("agents");
        builder
            .ingest_page(page(SnapshotSection::Artifacts, Vec::new()))
            .expect("artifacts");
        builder
            .ingest_page(page(
                SnapshotSection::Resources,
                vec![SnapshotItem::Resource(ResourceFacts {
                    id: resource_id,
                    task_id: Some(task_id),
                    owner_kind: OwnerKind::Task,
                    resource_kind: ResourceKind::Terminal,
                    recipe: ResourceRecipe::Terminal { cols: 40, rows: 8 },
                    lifecycle: ResourceLifecycle::Active,
                    runtime_generation: 1,
                    updated_at_ms: 1,
                })],
            ))
            .expect("resources");
        builder
            .ingest_page(page(SnapshotSection::Operations, Vec::new()))
            .expect("operations");
        (builder.finish().expect("client model"), task_id)
    }

    fn selected_task_follow_title_and_terminal_binding(cx: &mut gpui::App) {
        let (runtime, _shared) = TestRuntime::new(true, NativeHostActionResult::Queued);
        let (model, task_id) = terminal_bound_client_model();
        with_test_shell_in_app(cx, runtime, |shell| {
            assert!(!shell.terminal_state().is_live());
            shell
                .apply_client_model(Arc::new(model))
                .expect("apply model");
            let list = crate::ui::task_cockpit::TaskList::from_client_model_virtual(
                shell.client_model_snapshot().as_ref().unwrap(),
            )
            .expect("task list");
            let _ = shell.interaction.navigation_mouse_down(task_id, &list);
            shell.sync_cockpit_follow();
            assert_eq!(shell.cockpit().selected_task(), Some(task_id));
            let label = shell.task_row_label_for_test(task_id);
            assert!(
                label.contains("Bound terminal task") && label.contains("Idle"),
                "expected title/status projection, got {label}"
            );
            assert!(
                shell.terminal_state().is_live(),
                "complete terminal identity must bind the adapter"
            );
        });
    }

    fn dock_shortcuts_and_open_task_details_bind_selection(cx: &mut gpui::App) {
        use crate::ui::actions::{KeyboardShortcut, ShortcutKey};
        use crate::ui::task_cockpit::dock::DockTool as CockpitDockTool;

        let (runtime, shared) = TestRuntime::new(true, NativeHostActionResult::Queued);
        let (model, task_id) = terminal_bound_client_model();
        with_test_shell_in_app(cx, runtime, |shell| {
            shell
                .apply_client_model(Arc::new(model))
                .expect("apply model");
            let list = crate::ui::task_cockpit::TaskList::from_client_model_virtual(
                shell.client_model_snapshot().as_ref().unwrap(),
            )
            .expect("task list");
            let _ = shell.interaction.navigation_mouse_down(task_id, &list);
            shell.sync_cockpit_follow();

            shell.dispatch_keyboard_for_test(KeyboardShortcut::alt(ShortcutKey::Digit(4)));
            assert_eq!(
                shell.last_keyboard_action(),
                Some(crate::ui::actions::KeyboardAction::SelectDock(
                    crate::ui::actions::DockTool::Browser
                ))
            );
            assert_eq!(shell.cockpit().selected_task(), Some(task_id));
            assert_eq!(shell.cockpit().active_tool(), CockpitDockTool::Browser);

            shell.dispatch_keyboard_for_test(KeyboardShortcut::ctrl(ShortcutKey::Backtick));
            assert_eq!(
                shell.last_keyboard_action(),
                Some(crate::ui::actions::KeyboardAction::OpenTerminal)
            );
            assert_eq!(shell.cockpit().active_tool(), CockpitDockTool::Terminal);
            assert!(shell.interaction.keyboard_state().terminal_open);

            let before = shared.lock().expect("runtime").accepted.len();
            shell.dispatch_keyboard_for_test(KeyboardShortcut::ctrl(ShortcutKey::Character('m')));
            assert_eq!(
                shell.last_keyboard_action(),
                Some(crate::ui::actions::KeyboardAction::OpenTaskDetails)
            );
            assert!(shell.interaction.keyboard_state().task_details_open);
            let accepted = shared.lock().expect("runtime").accepted.clone();
            assert!(
                accepted.len() > before && accepted.iter().any(|record| {
                    matches!(
                        record.command,
                        super::NativeHostCommand::TaskShowQuery { task_id: id, .. } if id == task_id
                    )
                }),
                "OpenTaskDetails must dispatch TaskShowQuery for the selected task"
            );
        });
    }

    #[test]
    fn native_shell_follow_dock_and_details_scenarios() {
        let _test_guard = HEADLESS_SHELL_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("headless shell test lock");
        let completed = std::rc::Rc::new(std::cell::RefCell::new(false));
        let completed_for_app = std::rc::Rc::clone(&completed);
        gpui::Application::headless().run(move |cx| {
            crate::ui::init(cx);
            selected_task_follow_title_and_terminal_binding(cx);
            dock_shortcuts_and_open_task_details_bind_selection(cx);
            *completed_for_app.borrow_mut() = true;
            cx.quit();
        });
        assert!(
            *completed.borrow(),
            "follow/dock/details scenarios completed"
        );
    }
}

#[allow(dead_code)]
fn _terminal_adapter_dependency() -> &'static str {
    TERMINAL_ADAPTER_DEPENDENCY
}
