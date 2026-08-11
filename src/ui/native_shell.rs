//! The real native Task Cockpit shell entrypoint and its isolated host seam.
//!
//! This module deliberately owns only a local projection. It does not open the
//! installed profile, read the production session, start a legacy app, or
//! embed a WebView. A later inbox/host owner can provide a `ClientModel` and a
//! complete terminal model through the explicit seams below.

use std::cell::RefCell;
use std::env;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use gpui::{
    div, px, size, AnyElement, AppContext, Application, ClickEvent, Context, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseUpEvent,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window, WindowBounds, WindowOptions,
};
use gpui_component::button::{Button, ButtonVariants};

use crate::assets::AppAssets;
use crate::client::action;
use crate::client::{HostClient, HostClientConfig};
use crate::domain::id::TaskId;
use crate::domain::ClientId;
use crate::host::IpcError;
use crate::protocol::{Capability, CapabilitySet, FrameLimits};
use crate::ui::actions::{
    self, HostActions, HostStatus, KeyboardAction, KeyboardModel, KeyboardShortcut,
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
use crate::ui::task_cockpit::{TaskList, DEFAULT_VISIBLE_ROWS, FIXED_VIRTUAL_OVERSCAN};
use crate::ui::terminal_adapter::TerminalDockAdapter;
pub use crate::ui::terminal_adapter::{TerminalDockState, TERMINAL_ADAPTER_DEPENDENCY};
use crate::ui::tokens::{theme, Density, Scale, ThemeMode};

const NATIVE_PROFILE_DIR: &str = ".devmanager-next/dev-profile";
const NATIVE_PROFILE_NAME: &str = "native-next-dev";
const NATIVE_HOST_SCHEME: &str = "devtest";
const NATIVE_POINTER_ID: u64 = 1;
const MAX_RENDERED_TASK_ROWS: usize = DEFAULT_VISIBLE_ROWS + FIXED_VIRTUAL_OVERSCAN * 2;
const MAX_PENDING_HOST_ACTIONS: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeShellError {
    WorkspaceMissing { path: PathBuf },
    WorkspaceMustBeRoot { path: PathBuf },
    ProfileOverride { value: String },
    WindowOpen { message: String },
    HostConnect { message: String },
    HeadlessRenderFailed,
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
}

/// Result of handing one already-validated UI action to the host lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeHostActionResult {
    Queued,
    Disconnected,
    QueueFull,
}

/// The sole native-next transport/action owner.
///
/// GPUI paint and event callbacks never await [`HostClient`]. They enqueue a
/// bounded, typed [`NativeActionRecord`] here; the caller's controller/task
/// lane drains the records and uses the same client for command/query I/O.
/// Keeping the client in this one owner prevents header, inbox, and shell
/// attachments from silently opening a second connection.
pub struct NativeHostClientRuntime {
    client: HostClient,
    pending: std::collections::VecDeque<NativeActionRecord>,
}

impl std::fmt::Debug for NativeHostClientRuntime {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeHostClientRuntime")
            .field("connected", &self.client.is_connected())
            .field("pending_count", &self.pending.len())
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

    pub fn new(client: HostClient) -> Self {
        Self {
            client,
            pending: std::collections::VecDeque::new(),
        }
    }

    pub fn client(&self) -> &HostClient {
        &self.client
    }

    pub fn client_mut(&mut self) -> &mut HostClient {
        &mut self.client
    }

    pub fn is_connected(&self) -> bool {
        self.client.is_connected()
    }

    pub fn endpoint(&self) -> &str {
        self.client.endpoint()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Queue one action without performing transport work on the UI thread.
    pub fn enqueue(&mut self, action: NativeActionRecord) -> NativeHostActionResult {
        if !self.client.is_connected() {
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
        self.pending.drain(..).collect()
    }

    /// Execute a caller-created, revision-fenced command on this same client.
    ///
    /// The shell intentionally does not synthesize envelopes: task identity,
    /// expected revision, and command id belong to the host/controller lane.
    pub async fn execute_command(
        &mut self,
        envelope: crate::domain::command::CommandEnvelope,
    ) -> Result<crate::domain::command::CommandReceipt, IpcError> {
        self.client.execute_command(envelope).await
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

    fn begin_handler(&mut self, task_id: Option<TaskId>) -> (FocusEpoch, u64) {
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
        keyboard
            .activate(shortcut, &self.interaction, focus_epoch)
            .map(|action| (focus_epoch, request_generation, action))
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
        let event = ActionEvent::new(request, source, focus_epoch);
        Some(NativeActionRecord {
            id: descriptor.id,
            focus_epoch,
            request_generation,
            event,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessibilityNode {
    metadata: AccessibilityMetadata,
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
            children: Vec::new(),
        }
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
                );
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
        } else {
            AccessibilityNode::new(
                AccessibleRole::Status,
                "Task inbox ready",
                "Only the bounded visible task window is rendered.",
            )
        };
        let inbox = AccessibilityNode::new(
            AccessibleRole::Region,
            "Task inbox",
            "Bounded virtualized task list; keyboard and pointer navigation share one focus epoch.",
        )
        .with_children(
            std::iter::once(inbox_status)
                .chain(rows)
                .collect::<Vec<_>>(),
        );
        let toolbar = AccessibilityNode::new(
            AccessibleRole::Region,
            "Task cockpit actions",
            "Actions are dispatched through the shared client action catalog.",
        );
        let terminal = AccessibilityNode::new(
            AccessibleRole::Status,
            "Terminal dock",
            TerminalDockState::unavailable().message(),
        );
        let root = AccessibilityNode::new(
            AccessibleRole::Region,
            "Task Cockpit",
            "Native GPUI shell using an isolated dev/test host profile.",
        )
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeRenderSmokeReport {
    pub root_constructed: bool,
    pub semantic_nodes: usize,
    pub rendered_task_rows: usize,
    pub host_profile: PathBuf,
    pub profile_root: PathBuf,
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
            let entity = cx.new(|cx| NativeShell::new(profile.clone(), cx));
            let report = entity.update(cx, |shell, _cx| {
                let _root = shell.element_without_handlers();
                NativeRenderSmokeReport {
                    root_constructed: true,
                    semantic_nodes: shell.accessibility_tree.nodes().len(),
                    rendered_task_rows: shell.rendered_task_count(),
                    host_profile: shell.host_connection.profile_root().to_path_buf(),
                    profile_root: shell.profile.root().to_path_buf(),
                }
            });
            *report_slot_for_app.borrow_mut() = Some(report);
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
    host_runtime: Option<NativeHostClientRuntime>,
    task_list: TaskList,
    interaction: NativeInteraction,
    keyboard: KeyboardModel,
    accessibility_tree: AccessibilityTree,
    terminal: TerminalDockAdapter,
    focus_handle: FocusHandle,
}

impl NativeShell {
    pub fn new(profile: IsolatedDevProfile, cx: &mut Context<Self>) -> Self {
        Self::new_with_host_runtime(profile, None, cx)
    }

    pub fn new_with_host_runtime(
        profile: IsolatedDevProfile,
        host_runtime: Option<NativeHostClientRuntime>,
        cx: &mut Context<Self>,
    ) -> Self {
        let task_list = TaskList::empty();
        let accessibility_tree = AccessibilityTree::for_task_list(&task_list, None);
        Self {
            host_connection: profile.host_connection(),
            profile,
            host_runtime,
            task_list,
            interaction: NativeInteraction::new(None),
            keyboard: KeyboardModel::default(),
            accessibility_tree,
            terminal: TerminalDockAdapter::unavailable(),
            focus_handle: cx.focus_handle().tab_stop(true),
        }
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
            .map(NativeHostClientRuntime::endpoint)
            .unwrap_or_else(|| self.host_connection.endpoint())
    }

    pub fn host_runtime(&self) -> Option<&NativeHostClientRuntime> {
        self.host_runtime.as_ref()
    }

    pub fn host_runtime_mut(&mut self) -> Option<&mut NativeHostClientRuntime> {
        self.host_runtime.as_mut()
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
        self.host_runtime = Some(host_runtime);
        Ok(())
    }

    pub fn accessibility_tree(&self) -> &AccessibilityTree {
        &self.accessibility_tree
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
        let tokens = theme(ThemeMode::Dark, Density::Comfortable, Scale::Scale100);
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
                Button::new("native-shell-host-status")
                    .label("Host status")
                    .ghost(),
            )
            .child(
                div()
                    .text_color(tokens.text.secondary.to_gpui())
                    .whitespace_normal()
                    .child(format!("isolated dev/test host: {}", self.host_endpoint())),
            );
        let inbox = div()
            .id("native-shell-task-inbox")
            .w_full()
            .flex_col()
            .gap(px(tokens.density.spacing.xs))
            .overflow_y_scroll()
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
        let tokens = theme(ThemeMode::Dark, Density::Comfortable, Scale::Scale100);
        let metrics = tokens.density.physical();
        let task_rows = self
            .task_list
            .rendered_task_ids()
            .iter()
            .copied()
            .enumerate()
            .map(|(row_index, task_id)| {
                let handler = cx.listener(move |shell, event: &MouseDownEvent, window, cx| {
                    if event.button == MouseButton::Left {
                        cx.stop_propagation();
                        shell.focus_handle.focus(window);
                        let _ = shell
                            .interaction
                            .navigation_mouse_down(task_id, &shell.task_list);
                        shell.accessibility_tree = AccessibilityTree::for_task_list(
                            &shell.task_list,
                            shell.interaction.selected_task(),
                        );
                    }
                });
                let key_handler = cx.listener(move |shell, event: &KeyDownEvent, _window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        cx.stop_propagation();
                        let _ = shell
                            .interaction
                            .navigation_mouse_down(task_id, &shell.task_list);
                        shell.accessibility_tree = AccessibilityTree::for_task_list(
                            &shell.task_list,
                            shell.interaction.selected_task(),
                        );
                    }
                });
                div()
                    .id(("native-task-row", row_index))
                    .tab_stop(true)
                    .w_full()
                    .h(px(metrics.row_height as f32))
                    .p(px(metrics.row_padding as f32))
                    .border_b_1()
                    .border_color(tokens.borders.subtle.to_gpui())
                    .whitespace_normal()
                    .on_mouse_down(MouseButton::Left, handler)
                    .on_key_down(key_handler)
                    .child(format!("Task {task_id}"))
                    .into_any_element()
            })
            .collect::<Vec<_>>();

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
                        Button::new("native-shell-host-status")
                            .label("Host status")
                            .ghost()
                            .on_click(cx.listener(|shell, _event: &ClickEvent, _window, cx| {
                                cx.stop_propagation();
                                shell.dispatch_pointer_action(
                                    ActionRequest::HostStatus,
                                    NATIVE_POINTER_ID,
                                );
                            })),
                    )
                    .child(format!("isolated dev/test host: {}", self.host_endpoint())),
            )
            .child(
                div()
                    .id("native-shell-task-inbox")
                    .w_full()
                    .flex_col()
                    .gap(px(tokens.density.spacing.xs))
                    .overflow_y_scroll()
                    .children(task_rows),
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

    fn dispatch_action(&mut self, request: ActionRequest) {
        let Some(record) = self.interaction.action(request) else {
            return;
        };
        if let Some(runtime) = self.host_runtime.as_mut() {
            let _ = runtime.enqueue(record);
        }
    }

    fn dispatch_pointer_action(&mut self, request: ActionRequest, pointer_id: u64) {
        let Some(record) = self
            .interaction
            .action_from_source(request, ActivationSource::Pointer { pointer_id })
        else {
            return;
        };
        if let Some(runtime) = self.host_runtime.as_mut() {
            let _ = runtime.enqueue(record);
        }
    }

    fn dispatch_keyboard(&mut self, shortcut: KeyboardShortcut) {
        let _ = self.interaction.keyboard(&self.keyboard, shortcut);
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
    run_native_shell_with_runtime(workspace_root, None)
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
            let result = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::centered(size(px(1_280.0), px(800.0)), cx)),
                    ..WindowOptions::default()
                },
                move |_window, cx| {
                    cx.new(|cx| {
                        NativeShell::new_with_host_runtime(
                            profile_for_window,
                            host_runtime_for_window,
                            cx,
                        )
                    })
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

#[allow(dead_code)]
fn _terminal_adapter_dependency() -> &'static str {
    TERMINAL_ADAPTER_DEPENDENCY
}
