//! Single task-following context dock. UI holds no provider/PTY lifecycle.

use std::collections::BTreeMap;

use gpui::{div, rgb, IntoElement, ParentElement, Styled};

use crate::client::action::ActionRequest;
use crate::client::model::ClientModel;
use crate::domain::agent::AgentRole;
use crate::domain::id::{AgentSessionId, RequestId, ResourceId, TaskId};
use crate::domain::resource::{ResourceKind, ResourceLifecycle};
use crate::domain::snapshot::TaskSnapshot;
use crate::services::ProcessManager;
use crate::terminal::session::TerminalSessionView;
use crate::terminal::view::{
    render_terminal_surface, terminal_pane_from_replica, ReplicaPaneRequest, TerminalPaneModel,
    TerminalReplicaOverlay, TerminalScrollbarModel, TerminalSearchHighlight, TerminalSearchUiModel,
    TerminalSelectionSnapshot,
};
use crate::ui::components::empty_state::EmptyState;
use crate::ui::components::interaction::{
    redacted_bounded_text, AccessibilityMetadata, AccessibleRole, FocusEpoch, FocusEpochSource,
    KeyboardKey,
};
use crate::ui::tokens::ThemeTokens;

pub const DOCK_MIN_SIZE_RATIO: f32 = 0.18;
pub const DOCK_MAX_SIZE_RATIO: f32 = 0.55;
pub const DOCK_DEFAULT_SIZE_RATIO: f32 = 0.32;
pub const MAX_REMEMBERED_TASKS: usize = 256;
pub const MAX_REPLICA_ROWS: u16 = 256;
pub const MAX_REPLICA_COLS: u16 = 512;
pub const MAX_REPLICA_CELLS: usize = 256 * 512;
pub const MAX_SEARCH_SCALARS: usize = 256;
pub const MAX_EXIT_SUMMARY_SCALARS: usize = 160;

const DOCK_TOOLS: [DockTool; 7] = [
    DockTool::Changes,
    DockTool::Files,
    DockTool::Terminal,
    DockTool::Browser,
    DockTool::Services,
    DockTool::Artifacts,
    DockTool::Review,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DockTool {
    Changes,
    Files,
    Terminal,
    Browser,
    Services,
    Artifacts,
    Review,
}

impl DockTool {
    pub fn label(self) -> &'static str {
        match self {
            Self::Changes => "Changes",
            Self::Files => "Files",
            Self::Terminal => "Terminal",
            Self::Browser => "Browser",
            Self::Services => "Services",
            Self::Artifacts => "Artifacts",
            Self::Review => "Review",
        }
    }

    fn from_alt_index(index: u8) -> Option<Self> {
        DOCK_TOOLS.get((index as usize).saturating_sub(1)).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockEdge {
    Right,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockPointerSurface {
    Sidebar,
    Tab(DockTool),
    ResizeHandle,
    TerminalGrid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSurfaceState {
    Live,
    Reconnecting,
    Resyncing,
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalPresentation {
    Semantic,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockUnavailableReason {
    NoTaskSelected,
    MissingHostProjection,
    NoMatchingTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockUnavailable {
    pub tool: DockTool,
    pub reason: DockUnavailableReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockProjectionError {
    NoTaskSelected,
    Unbound,
    BindingMismatch,
    GenerationMismatch {
        expected_runtime: u64,
        actual_runtime: u64,
        expected_resource: u64,
        actual_resource: u64,
    },
    ForeignIdentity,
    ZeroSequence,
    RegressedSequence {
        last: u64,
        actual: u64,
    },
    SequenceGap {
        last: u64,
        actual: u64,
    },
    SnapshotExceedsBounds,
    OverlayViewRejected,
    NonFiniteSize,
    StaleActionNode,
    NeedsResync,
    DuplicateRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyUnavailable {
    RuntimeCensus,
    HostTerminalStream,
    LiveRuntimeCensus,
    PtyInput,
    NativeShellMount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalRuntimeIdentity {
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    resource_id: ResourceId,
    runtime_generation: u64,
    resource_generation: u64,
}

impl TerminalRuntimeIdentity {
    pub fn new(
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        resource_id: ResourceId,
        runtime_generation: u64,
        resource_generation: u64,
    ) -> Self {
        Self {
            task_id,
            agent_session_id,
            resource_id,
            runtime_generation,
            resource_generation,
        }
    }

    pub fn task_id(self) -> TaskId {
        self.task_id
    }

    pub fn agent_session_id(self) -> AgentSessionId {
        self.agent_session_id
    }

    pub fn resource_id(self) -> ResourceId {
        self.resource_id
    }

    pub fn runtime_generation(self) -> u64 {
        self.runtime_generation
    }

    pub fn resource_generation(self) -> u64 {
        self.resource_generation
    }

    fn ids_match(self, other: Self) -> bool {
        self.task_id == other.task_id
            && self.agent_session_id == other.agent_session_id
            && self.resource_id == other.resource_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTerminalBinding {
    identity: TerminalRuntimeIdentity,
}

impl HostTerminalBinding {
    fn from_task_snapshot(snapshot: &TaskSnapshot) -> Result<Self, DockProjectionError> {
        let primary_id = snapshot
            .primary_agent_id
            .ok_or(DockProjectionError::Unbound)?;
        let agent = snapshot
            .agents
            .get(&primary_id)
            .ok_or(DockProjectionError::Unbound)?;
        if agent.task_id != snapshot.task.id || agent.id != primary_id {
            return Err(DockProjectionError::BindingMismatch);
        }
        if !matches!(agent.role, AgentRole::Primary) {
            return Err(DockProjectionError::Unbound);
        }
        let mut terminals = snapshot.resources.values().filter(|resource| {
            resource.task_id == Some(snapshot.task.id)
                && resource.resource_kind == ResourceKind::Terminal
                && resource.lifecycle == ResourceLifecycle::Active
        });
        let resource = terminals.next().ok_or(DockProjectionError::Unbound)?;
        if terminals.next().is_some() {
            return Err(DockProjectionError::BindingMismatch);
        }
        Ok(Self {
            identity: TerminalRuntimeIdentity {
                task_id: snapshot.task.id,
                agent_session_id: agent.id,
                resource_id: resource.id,
                runtime_generation: agent.runtime_generation,
                resource_generation: resource.runtime_generation,
            },
        })
    }

    pub fn from_client_model(
        model: &ClientModel,
        task_id: TaskId,
    ) -> Result<Self, DockProjectionError> {
        let snapshot = model
            .tasks()
            .get(&task_id)
            .ok_or(DockProjectionError::ForeignIdentity)?;
        Self::from_task_snapshot(snapshot)
    }

    pub fn identity(self) -> TerminalRuntimeIdentity {
        self.identity
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostStreamCursor {
    identity: TerminalRuntimeIdentity,
    sequence: u64,
    full_snapshot: bool,
}

impl HostStreamCursor {
    pub fn from_identity(
        identity: TerminalRuntimeIdentity,
        sequence: u64,
        full_snapshot: bool,
    ) -> Self {
        Self {
            identity,
            sequence,
            full_snapshot,
        }
    }

    pub fn delta(
        model: &ClientModel,
        task_id: TaskId,
        sequence: u64,
    ) -> Result<Self, DockProjectionError> {
        Self::from_model(model, task_id, sequence, false)
    }

    pub fn full_snapshot(
        model: &ClientModel,
        task_id: TaskId,
        sequence: u64,
    ) -> Result<Self, DockProjectionError> {
        Self::from_model(model, task_id, sequence, true)
    }

    fn from_model(
        model: &ClientModel,
        task_id: TaskId,
        sequence: u64,
        full_snapshot: bool,
    ) -> Result<Self, DockProjectionError> {
        Ok(Self {
            identity: HostTerminalBinding::from_client_model(model, task_id)?.identity(),
            sequence,
            full_snapshot,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostAdmitReport {
    stream: Result<(), DependencyUnavailable>,
}

impl HostAdmitReport {
    fn host_stream_hold() -> Self {
        Self {
            stream: Err(DependencyUnavailable::HostTerminalStream),
        }
    }

    fn host_stream_admitted() -> Self {
        Self { stream: Ok(()) }
    }

    pub fn stream(self) -> Result<(), DependencyUnavailable> {
        self.stream
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeCensusSnapshot {
    provider_roots_before: u64,
    provider_roots_after: u64,
    pty_readers_before: u64,
    pty_readers_after: u64,
}

impl RuntimeCensusSnapshot {
    pub fn unchanged(self) -> bool {
        self.provider_roots_before == self.provider_roots_after
            && self.pty_readers_before == self.pty_readers_after
    }

    pub fn provider_roots_before(self) -> u64 {
        self.provider_roots_before
    }

    pub fn pty_readers_before(self) -> u64 {
        self.pty_readers_before
    }
}

pub struct ProcessManagerCensus<'a> {
    manager: &'a ProcessManager,
}

impl<'a> ProcessManagerCensus<'a> {
    pub fn new(manager: &'a ProcessManager) -> Self {
        Self { manager }
    }

    pub fn provider_root_count(&self) -> u64 {
        self.manager
            .runtime_state()
            .sessions
            .values()
            .filter(|session| session.session_kind.is_ai())
            .count() as u64
    }

    pub fn pty_reader_count(&self) -> u64 {
        self.manager.all_session_views().len() as u64
    }

    pub fn one_provider_one_pty_proof(&self) -> Result<(), DependencyUnavailable> {
        let _ = (self.provider_root_count(), self.pty_reader_count());
        Err(DependencyUnavailable::LiveRuntimeCensus)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewSwitchReport {
    identity: Option<TerminalRuntimeIdentity>,
    census: Result<RuntimeCensusSnapshot, DependencyUnavailable>,
}

impl ViewSwitchReport {
    pub fn identity(self) -> Option<TerminalRuntimeIdentity> {
        self.identity
    }

    pub fn census(self) -> Result<RuntimeCensusSnapshot, DependencyUnavailable> {
        self.census
    }
}

#[derive(Debug, Clone)]
pub struct TerminalViewport {
    pub selection: Option<TerminalSelectionSnapshot>,
    pub search: Option<TerminalSearchUiModel>,
    pub search_highlight: Option<TerminalSearchHighlight>,
    pub scrollbar: Option<TerminalScrollbarModel>,
    pub focused: bool,
}

impl Default for TerminalViewport {
    fn default() -> Self {
        Self {
            selection: None,
            search: None,
            search_highlight: None,
            scrollbar: None,
            focused: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionEpoch {
    sequence: u64,
}

impl ActionEpoch {
    const fn initial() -> Self {
        Self { sequence: 0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DockPressOwner {
    task_id: TaskId,
    agent_session_id: Option<AgentSessionId>,
    resource_id: Option<ResourceId>,
    runtime_generation: Option<u64>,
    resource_generation: Option<u64>,
    focus_epoch: FocusEpoch,
    pointer_id: u64,
    button: PointerButton,
    surface: DockPointerSurface,
    tool: DockTool,
    action_epoch: ActionEpoch,
}

impl DockPressOwner {
    pub fn pointer_id(self) -> u64 {
        self.pointer_id
    }

    pub fn surface(self) -> DockPointerSurface {
        self.surface
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerPress {
    pub pointer_id: u64,
    pub button: PointerButton,
    pub surface: DockPointerSurface,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockActionDispatch {
    catalog_request: ActionRequest,
    tool: Option<DockTool>,
    toggle_raw: bool,
    escape: bool,
    task_id: TaskId,
    request_id: RequestId,
    focus_epoch: FocusEpoch,
    action_epoch: ActionEpoch,
    agent_session_id: Option<AgentSessionId>,
    resource_id: Option<ResourceId>,
    runtime_generation: Option<u64>,
    resource_generation: Option<u64>,
}

impl DockActionDispatch {
    pub fn catalog_request(&self) -> &ActionRequest {
        &self.catalog_request
    }

    pub fn request_id(&self) -> RequestId {
        self.request_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerPhase {
    Down,
    Move,
    Up,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockShortcut {
    AltTool(u8),
    ToggleRawTerminal,
    Escape,
}

#[derive(Debug, Clone)]
pub struct DockTabProjection {
    pub tool: DockTool,
    pub name: String,
    pub selected: bool,
    pub unavailable: bool,
    pub shortcut: DockShortcut,
    pub accessibility: AccessibilityMetadata,
}

#[derive(Debug, Clone)]
pub struct DockResizeHandleProjection {
    pub accessibility: AccessibilityMetadata,
    pub size_ratio: f32,
}

#[derive(Debug, Clone)]
pub struct DockChromeProjection {
    pub edge: DockEdge,
    pub collapsed: bool,
    pub tabs: Vec<DockTabProjection>,
    pub tab_list: AccessibilityMetadata,
    pub active_tool: DockTool,
    pub unavailable: Option<DockUnavailable>,
    pub resize_handle: Option<DockResizeHandleProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockProjectionFingerprint {
    pub last_sequence: u64,
    pub surface_state: TerminalSurfaceState,
    pub exit_summary: Option<String>,
    pub screen_text: Option<String>,
    pub runtime_generation: Option<u64>,
    pub resource_generation: Option<u64>,
    pub needs_resync: bool,
    pub tool: DockTool,
    pub presentation: TerminalPresentation,
}

#[derive(Clone)]
struct RememberedDockState {
    tool: DockTool,
    terminal_presentation: TerminalPresentation,
    size_ratio: f32,
    collapsed: bool,
    viewport: TerminalViewport,
    identity: Option<TerminalRuntimeIdentity>,
    last_sequence: u64,
    surface_state: TerminalSurfaceState,
    exit_summary: Option<String>,
    /// The most recent complete native terminal view admitted for this exact
    /// task/resource/runtime fence. This is client presentation state only;
    /// the dock never owns the PTY or provider process.
    replica_view: Option<TerminalSessionView>,
    /// Last complete view retained while a bounded reconnect/resync/exit
    /// overlay is shown. It is never used for a different identity.
    last_valid_view: Option<TerminalSessionView>,
}

impl RememberedDockState {
    fn default_state() -> Self {
        Self {
            tool: DockTool::Files,
            terminal_presentation: TerminalPresentation::Semantic,
            size_ratio: DOCK_DEFAULT_SIZE_RATIO,
            collapsed: false,
            viewport: TerminalViewport::default(),
            identity: None,
            last_sequence: 0,
            surface_state: TerminalSurfaceState::Live,
            exit_summary: None,
            replica_view: None,
            last_valid_view: None,
        }
    }
}

/// Client-local dock. Host projections are applied; the dock never owns a PTY.
pub struct ContextDock {
    edge: DockEdge,
    selected_task: Option<TaskId>,
    remembered: BTreeMap<TaskId, RememberedDockState>,
    remembered_order: Vec<TaskId>,
    focus: FocusEpochSource,
    action_epoch: ActionEpoch,
    press_owner: Option<DockPressOwner>,
    needs_resync: bool,
    focused_tab_index: Option<usize>,
    terminal_mouse_report_emitted: bool,
    terminal_selection_changed: bool,
    terminal_click_completed: bool,
    last_request_id: Option<RequestId>,
}

impl ContextDock {
    pub fn new(edge: DockEdge) -> Self {
        Self {
            edge,
            selected_task: None,
            remembered: BTreeMap::new(),
            remembered_order: Vec::new(),
            focus: FocusEpochSource::new(),
            action_epoch: ActionEpoch::initial(),
            press_owner: None,
            needs_resync: false,
            focused_tab_index: None,
            terminal_mouse_report_emitted: false,
            terminal_selection_changed: false,
            terminal_click_completed: false,
            last_request_id: None,
        }
    }

    pub fn tools() -> &'static [DockTool] {
        &DOCK_TOOLS
    }

    pub fn placement_for_aspect(width: f32, height: f32, preference: Option<DockEdge>) -> DockEdge {
        if let Some(edge) = preference {
            return edge;
        }
        if width >= height {
            DockEdge::Right
        } else {
            DockEdge::Bottom
        }
    }

    pub fn edge(&self) -> DockEdge {
        self.edge
    }

    pub fn set_edge(&mut self, edge: DockEdge) {
        if self.edge != edge {
            self.edge = edge;
            self.advance_epochs();
        }
    }

    pub fn selected_task(&self) -> Option<TaskId> {
        self.selected_task
    }

    pub fn active_tool(&self) -> DockTool {
        self.current_memory().tool
    }

    pub fn terminal_presentation(&self) -> TerminalPresentation {
        self.current_memory().terminal_presentation
    }

    pub fn showing_raw_terminal(&self) -> bool {
        self.active_tool() == DockTool::Terminal
            && self.terminal_presentation() == TerminalPresentation::Raw
    }

    pub fn is_collapsed(&self) -> bool {
        self.current_memory().collapsed
    }

    pub fn size_ratio(&self) -> f32 {
        self.current_memory().size_ratio
    }

    pub fn needs_resync(&self) -> bool {
        self.needs_resync
    }

    pub fn follow_task(&mut self, task_id: TaskId) {
        if self.selected_task == Some(task_id) {
            return;
        }
        self.selected_task = Some(task_id);
        self.remember_task(task_id);
        self.press_owner = None;
        self.focused_tab_index = None;
        self.terminal_mouse_report_emitted = false;
        self.terminal_selection_changed = false;
        self.terminal_click_completed = false;
        self.advance_epochs();
    }

    fn select_tool(&mut self, tool: DockTool) {
        if self.selected_task.is_none() {
            return;
        }
        self.with_memory(|memory| {
            memory.tool = tool;
            if tool != DockTool::Terminal {
                memory.viewport.focused = false;
            }
        });
        self.focused_tab_index = DOCK_TOOLS.iter().position(|candidate| *candidate == tool);
        self.advance_epochs();
    }

    fn set_terminal_presentation(&mut self, presentation: TerminalPresentation) {
        if self.selected_task.is_none() {
            return;
        }
        self.with_memory(|memory| {
            memory.tool = DockTool::Terminal;
            memory.terminal_presentation = presentation;
            memory.viewport.focused = presentation == TerminalPresentation::Raw;
        });
        self.advance_epochs();
    }

    pub fn collapse(&mut self) {
        self.with_memory(|memory| {
            memory.collapsed = true;
            memory.viewport.focused = false;
        });
        self.press_owner = None;
        self.terminal_click_completed = false;
        self.advance_epochs();
    }

    pub fn reopen(&mut self) {
        self.with_memory(|memory| memory.collapsed = false);
        self.advance_epochs();
    }

    pub fn resize(&mut self, size_ratio: f32) -> Result<(), DockProjectionError> {
        if !size_ratio.is_finite() {
            return Err(DockProjectionError::NonFiniteSize);
        }
        if self.selected_task.is_none() {
            return Err(DockProjectionError::NoTaskSelected);
        }
        let clamped = size_ratio.clamp(DOCK_MIN_SIZE_RATIO, DOCK_MAX_SIZE_RATIO);
        self.with_memory(|memory| memory.size_ratio = clamped);
        Ok(())
    }

    pub fn tool_availability(&self, tool: DockTool) -> Result<(), DockUnavailable> {
        let Some(_) = self.selected_task else {
            return Err(DockUnavailable {
                tool,
                reason: DockUnavailableReason::NoTaskSelected,
            });
        };
        match tool {
            DockTool::Terminal => {
                if self.current_memory().identity.is_some() {
                    Ok(())
                } else {
                    Err(DockUnavailable {
                        tool,
                        reason: DockUnavailableReason::NoMatchingTerminal,
                    })
                }
            }
            _ => Err(DockUnavailable {
                tool,
                reason: DockUnavailableReason::MissingHostProjection,
            }),
        }
    }

    pub fn bind_from_projection(
        &mut self,
        snapshot: &TaskSnapshot,
    ) -> Result<(), DockProjectionError> {
        let _ = snapshot;
        Err(DockProjectionError::ForeignIdentity)
    }

    pub fn bind_from_model(&mut self, model: &ClientModel) -> Result<(), DockProjectionError> {
        let Some(task_id) = self.selected_task else {
            return Err(DockProjectionError::NoTaskSelected);
        };
        let identity = HostTerminalBinding::from_client_model(model, task_id)?.identity();
        let previous = self.current_memory().identity;
        if let Some(previous) = previous {
            if !previous.ids_match(identity) {
                return Err(DockProjectionError::BindingMismatch);
            }
            if previous.runtime_generation != identity.runtime_generation
                || previous.resource_generation != identity.resource_generation
            {
                self.with_memory(|memory| {
                    memory.viewport = TerminalViewport::default();
                    memory.last_sequence = 0;
                    memory.surface_state = TerminalSurfaceState::Live;
                    memory.exit_summary = None;
                    memory.identity = Some(identity);
                    memory.terminal_presentation = TerminalPresentation::Semantic;
                    memory.replica_view = None;
                    memory.last_valid_view = None;
                });
                self.needs_resync = true;
                self.press_owner = None;
                self.terminal_click_completed = false;
                self.advance_epochs();
                return Ok(());
            }
        }
        self.with_memory(|memory| memory.identity = Some(identity));
        Ok(())
    }

    pub fn admit_host_cursor(
        &mut self,
        cursor: HostStreamCursor,
    ) -> Result<HostAdmitReport, DockProjectionError> {
        let expected = self
            .current_memory()
            .identity
            .ok_or(DockProjectionError::Unbound)?;
        if cursor.identity.task_id != expected.task_id
            || cursor.identity.agent_session_id != expected.agent_session_id
            || cursor.identity.resource_id != expected.resource_id
        {
            return Err(DockProjectionError::ForeignIdentity);
        }
        if cursor.identity.runtime_generation != expected.runtime_generation
            || cursor.identity.resource_generation != expected.resource_generation
        {
            return Err(DockProjectionError::GenerationMismatch {
                expected_runtime: expected.runtime_generation,
                actual_runtime: cursor.identity.runtime_generation,
                expected_resource: expected.resource_generation,
                actual_resource: cursor.identity.resource_generation,
            });
        }
        if cursor.sequence == 0 {
            return Err(DockProjectionError::ZeroSequence);
        }
        if self.needs_resync && !cursor.full_snapshot {
            return Err(DockProjectionError::NeedsResync);
        }
        let last = self.current_memory().last_sequence;
        if cursor.full_snapshot {
            if last != 0 && cursor.sequence <= last {
                return Err(DockProjectionError::RegressedSequence {
                    last,
                    actual: cursor.sequence,
                });
            }
        } else {
            if last == 0 && cursor.sequence != 1 {
                self.mark_needs_resync();
                return Err(DockProjectionError::SequenceGap {
                    last,
                    actual: cursor.sequence,
                });
            }
            if last != 0 {
                if cursor.sequence <= last {
                    return Err(DockProjectionError::RegressedSequence {
                        last,
                        actual: cursor.sequence,
                    });
                }
                if cursor.sequence != last + 1 {
                    self.mark_needs_resync();
                    return Err(DockProjectionError::SequenceGap {
                        last,
                        actual: cursor.sequence,
                    });
                }
            }
        }
        self.with_memory(|memory| {
            memory.last_sequence = cursor.sequence;
            if cursor.full_snapshot {
                memory.surface_state = TerminalSurfaceState::Live;
                memory.exit_summary = None;
            }
        });
        self.needs_resync = false;
        Ok(HostAdmitReport::host_stream_admitted())
    }

    /// Admit a complete native terminal view from the task-owned stream.
    ///
    /// The cursor performs all identity, generation, and sequence fencing;
    /// only a view admitted by that fence reaches the renderer. A complete
    /// view is required for every update because the native renderer consumes
    /// a coherent screen snapshot rather than a loosely correlated cell list.
    pub fn admit_host_view(
        &mut self,
        cursor: HostStreamCursor,
        view: TerminalSessionView,
    ) -> Result<HostAdmitReport, DockProjectionError> {
        let full_snapshot = cursor.full_snapshot;
        self.admit_host_cursor(cursor)?;
        self.with_memory(|memory| {
            memory.replica_view = Some(view.clone());
            memory.last_valid_view = Some(view);
            if full_snapshot {
                memory.surface_state = TerminalSurfaceState::Live;
                memory.exit_summary = None;
            }
        });
        Ok(HostAdmitReport::host_stream_admitted())
    }

    /// Convenience seam for the native shell's task-owned subscription. The
    /// client model remains the authority for task/session/resource identity;
    /// the stream is allowed to supply only sequence/full-snapshot metadata
    /// and the already-materialized native view.
    pub fn admit_host_view_from_model(
        &mut self,
        model: &ClientModel,
        task_id: TaskId,
        sequence: u64,
        full_snapshot: bool,
        view: TerminalSessionView,
    ) -> Result<HostAdmitReport, DockProjectionError> {
        let cursor = if full_snapshot {
            HostStreamCursor::full_snapshot(model, task_id, sequence)?
        } else {
            HostStreamCursor::delta(model, task_id, sequence)?
        };
        self.admit_host_view(cursor, view)
    }

    pub fn present_host_overlay(
        &mut self,
        model: &ClientModel,
        surface_state: TerminalSurfaceState,
        exit_summary: Option<&str>,
    ) -> Result<(), DockProjectionError> {
        if surface_state == TerminalSurfaceState::Live {
            return Err(DockProjectionError::OverlayViewRejected);
        }
        let expected = self
            .current_memory()
            .identity
            .ok_or(DockProjectionError::Unbound)?;
        let Some(task_id) = self.selected_task else {
            return Err(DockProjectionError::NoTaskSelected);
        };
        let presented = HostTerminalBinding::from_client_model(model, task_id)?.identity();
        if presented != expected {
            if !presented.ids_match(expected) {
                return Err(DockProjectionError::ForeignIdentity);
            }
            return Err(DockProjectionError::GenerationMismatch {
                expected_runtime: expected.runtime_generation,
                actual_runtime: presented.runtime_generation,
                expected_resource: expected.resource_generation,
                actual_resource: presented.resource_generation,
            });
        }
        let summary = exit_summary
            .map(bound_exit_summary)
            .filter(|value| !value.is_empty());
        self.with_memory(|memory| {
            memory.surface_state = surface_state;
            memory.exit_summary = summary;
        });
        Ok(())
    }

    pub fn switch_to_semantic(
        &mut self,
        model: &ClientModel,
        census: Option<&ProcessManagerCensus<'_>>,
    ) -> Result<ViewSwitchReport, DockProjectionError> {
        self.switch_presentation(model, TerminalPresentation::Semantic, census)
    }

    pub fn switch_to_raw_terminal(
        &mut self,
        model: &ClientModel,
        census: Option<&ProcessManagerCensus<'_>>,
    ) -> Result<ViewSwitchReport, DockProjectionError> {
        self.switch_presentation(model, TerminalPresentation::Raw, census)
    }

    fn switch_presentation(
        &mut self,
        model: &ClientModel,
        presentation: TerminalPresentation,
        census: Option<&ProcessManagerCensus<'_>>,
    ) -> Result<ViewSwitchReport, DockProjectionError> {
        if self.needs_resync {
            return Err(DockProjectionError::NeedsResync);
        }
        let identity = self.require_bound_model_identity(model)?;
        let census = snapshot_census(census);
        self.set_terminal_presentation(presentation);
        Ok(ViewSwitchReport {
            identity: Some(identity),
            census,
        })
    }

    fn require_bound_model_identity(
        &self,
        model: &ClientModel,
    ) -> Result<TerminalRuntimeIdentity, DockProjectionError> {
        let expected = self
            .current_memory()
            .identity
            .ok_or(DockProjectionError::Unbound)?;
        let presented = HostTerminalBinding::from_client_model(model, expected.task_id)?.identity();
        if presented != expected {
            if !presented.ids_match(expected) {
                return Err(DockProjectionError::ForeignIdentity);
            }
            return Err(DockProjectionError::GenerationMismatch {
                expected_runtime: expected.runtime_generation,
                actual_runtime: presented.runtime_generation,
                expected_resource: expected.resource_generation,
                actual_resource: presented.resource_generation,
            });
        }
        Ok(expected)
    }

    pub fn terminal_binding(&self) -> Option<HostTerminalBinding> {
        self.current_memory()
            .identity
            .map(|identity| HostTerminalBinding { identity })
    }

    pub fn replica_view(&self) -> Option<TerminalSessionView> {
        self.current_memory().replica_view
    }

    pub fn last_valid_view(&self) -> Option<TerminalSessionView> {
        self.current_memory().last_valid_view
    }

    pub fn emit_terminal_mouse_to_host(&self) -> Result<(), DependencyUnavailable> {
        Err(DependencyUnavailable::PtyInput)
    }

    pub fn viewport(&self) -> TerminalViewport {
        self.current_memory().viewport
    }

    pub fn set_viewport(&mut self, mut viewport: TerminalViewport) {
        if let Some(search) = viewport.search.as_mut() {
            search.query = bound_search_query(&search.query);
            search.summary = bound_search_query(&search.summary);
        }
        self.with_memory(|memory| memory.viewport = viewport);
    }

    pub fn projection_fingerprint(&self) -> DockProjectionFingerprint {
        let memory = self.current_memory();
        DockProjectionFingerprint {
            last_sequence: memory.last_sequence,
            surface_state: memory.surface_state,
            exit_summary: memory.exit_summary.clone(),
            screen_text: None,
            runtime_generation: memory.identity.map(|identity| identity.runtime_generation),
            resource_generation: memory.identity.map(|identity| identity.resource_generation),
            needs_resync: self.needs_resync,
            tool: memory.tool,
            presentation: memory.terminal_presentation,
        }
    }

    pub fn focus_epoch(&self) -> FocusEpoch {
        self.focus.current()
    }

    pub fn action_epoch(&self) -> ActionEpoch {
        self.action_epoch
    }

    pub fn focus_terminal(&mut self) {
        if self.needs_resync || !self.showing_raw_terminal() || self.is_collapsed() {
            return;
        }
        self.with_memory(|memory| memory.viewport.focused = true);
        self.press_owner = None;
        self.terminal_click_completed = false;
        self.terminal_mouse_report_emitted = false;
        self.advance_epochs();
    }

    pub fn pointer_down(&mut self, press: PointerPress) -> bool {
        if self.needs_resync {
            return false;
        }
        let Some(task_id) = self.selected_task else {
            return false;
        };
        match press.surface {
            DockPointerSurface::Tab(_) => {
                self.advance_epochs();
                self.capture_press(task_id, press);
                true
            }
            DockPointerSurface::ResizeHandle | DockPointerSurface::Sidebar => {
                self.advance_epochs();
                self.with_memory(|memory| memory.viewport.focused = false);
                self.terminal_click_completed = false;
                self.capture_press(task_id, press);
                true
            }
            DockPointerSurface::TerminalGrid => {
                if !self.showing_raw_terminal()
                    || self.is_collapsed()
                    || !self.current_memory().viewport.focused
                {
                    return false;
                }
                self.capture_press(task_id, press);
                true
            }
        }
    }

    pub fn pointer_move(&mut self, press: PointerPress) -> bool {
        if self.needs_resync {
            return false;
        }
        let Some(owner) = self.press_owner else {
            return false;
        };
        if !self.owner_matches_current(owner) || !Self::press_fields_match(owner, press) {
            return false;
        }
        match owner.surface {
            DockPointerSurface::TerminalGrid => {
                self.terminal_selection_changed = true;
                true
            }
            DockPointerSurface::ResizeHandle => true,
            DockPointerSurface::Tab(_) | DockPointerSurface::Sidebar => false,
        }
    }

    pub fn pointer_up(&mut self, press: PointerPress) -> bool {
        if self.needs_resync {
            return false;
        }
        let Some(owner) = self.press_owner else {
            return false;
        };
        if !self.owner_matches_current(owner) || !Self::press_fields_match(owner, press) {
            return false;
        }
        self.press_owner = None;
        if owner.surface == DockPointerSurface::TerminalGrid {
            self.terminal_click_completed = true;
            self.terminal_mouse_report_emitted = true;
        }
        true
    }

    pub fn pointer_cancel(&mut self, press: PointerPress) -> bool {
        if self.needs_resync {
            return false;
        }
        let Some(owner) = self.press_owner else {
            return false;
        };
        if !self.owner_matches_current(owner) || !Self::press_fields_match(owner, press) {
            return false;
        }
        self.press_owner = None;
        true
    }

    pub fn release_press(&mut self, owner: DockPressOwner) -> bool {
        if self.needs_resync {
            return false;
        }
        let Some(current) = self.press_owner else {
            return false;
        };
        if current != owner || !self.owner_matches_current(owner) {
            return false;
        }
        self.press_owner = None;
        true
    }

    pub fn press_owner(&self) -> Option<DockPressOwner> {
        self.press_owner
    }

    pub fn terminal_mouse_reports_enabled(&self) -> bool {
        self.terminal_click_completed && !self.is_collapsed() && self.showing_raw_terminal()
    }

    pub fn terminal_mouse_report_emitted(&self) -> bool {
        self.terminal_mouse_report_emitted
    }

    pub fn terminal_selection_changed(&self) -> bool {
        self.terminal_selection_changed
    }

    pub fn capture_action(
        &self,
        tool: DockTool,
        request_id: RequestId,
    ) -> Result<DockActionDispatch, DockProjectionError> {
        self.capture_dispatch(Some(tool), false, false, request_id)
    }

    pub fn dispatch_shortcut(
        &mut self,
        shortcut: DockShortcut,
        request_id: RequestId,
        model: &ClientModel,
    ) -> Result<(), DockProjectionError> {
        let dispatch = match shortcut {
            DockShortcut::AltTool(index) => {
                let tool = DockTool::from_alt_index(index).ok_or(DockProjectionError::Unbound)?;
                self.capture_dispatch(Some(tool), false, false, request_id)?
            }
            DockShortcut::ToggleRawTerminal => {
                self.capture_dispatch(None, true, false, request_id)?
            }
            DockShortcut::Escape => self.capture_dispatch(None, false, true, request_id)?,
        };
        self.dispatch_action(dispatch, model)
    }

    pub fn dispatch_action(
        &mut self,
        dispatch: DockActionDispatch,
        model: &ClientModel,
    ) -> Result<(), DockProjectionError> {
        if self.needs_resync {
            return Err(DockProjectionError::NeedsResync);
        }
        if dispatch.focus_epoch != self.focus.current()
            || dispatch.action_epoch != self.action_epoch
        {
            return Err(DockProjectionError::StaleActionNode);
        }
        if self.last_request_id == Some(dispatch.request_id) {
            return Err(DockProjectionError::DuplicateRequest);
        }
        let Some(task_id) = self.selected_task else {
            return Err(DockProjectionError::NoTaskSelected);
        };
        if task_id != dispatch.task_id {
            return Err(DockProjectionError::ForeignIdentity);
        }
        match dispatch.catalog_request {
            ActionRequest::TaskShow {
                task_id: request_task,
            } if request_task == task_id => {}
            _ => return Err(DockProjectionError::BindingMismatch),
        }
        if let Some(current) = self.current_memory().identity {
            let presented = HostTerminalBinding::from_client_model(model, task_id)?.identity();
            if dispatch.agent_session_id != Some(current.agent_session_id)
                || dispatch.resource_id != Some(current.resource_id)
                || dispatch.runtime_generation != Some(current.runtime_generation)
                || dispatch.resource_generation != Some(current.resource_generation)
                || presented != current
            {
                return Err(DockProjectionError::BindingMismatch);
            }
        }
        self.last_request_id = Some(dispatch.request_id);
        if dispatch.escape {
            self.focused_tab_index = None;
            self.press_owner = None;
            self.advance_epochs();
            return Ok(());
        }
        if dispatch.toggle_raw {
            let next = if self.showing_raw_terminal() {
                TerminalPresentation::Semantic
            } else {
                TerminalPresentation::Raw
            };
            self.set_terminal_presentation(next);
            return Ok(());
        }
        if let Some(tool) = dispatch.tool {
            self.select_tool(tool);
        }
        Ok(())
    }

    fn capture_dispatch(
        &self,
        tool: Option<DockTool>,
        toggle_raw: bool,
        escape: bool,
        request_id: RequestId,
    ) -> Result<DockActionDispatch, DockProjectionError> {
        if self.needs_resync {
            return Err(DockProjectionError::NeedsResync);
        }
        let task_id = self
            .selected_task
            .ok_or(DockProjectionError::NoTaskSelected)?;
        let identity = self.current_memory().identity;
        Ok(DockActionDispatch {
            catalog_request: ActionRequest::TaskShow { task_id },
            tool,
            toggle_raw,
            escape,
            task_id,
            request_id,
            focus_epoch: self.focus.current(),
            action_epoch: self.action_epoch,
            agent_session_id: identity.map(|identity| identity.agent_session_id),
            resource_id: identity.map(|identity| identity.resource_id),
            runtime_generation: identity.map(|identity| identity.runtime_generation),
            resource_generation: identity.map(|identity| identity.resource_generation),
        })
    }

    pub fn handle_key(&mut self, key: KeyboardKey) {
        if key != KeyboardKey::Tab {
            return;
        }
        let next = match self.focused_tab_index {
            Some(index) => (index + 1) % DOCK_TOOLS.len(),
            None => 0,
        };
        self.focused_tab_index = Some(next);
    }

    pub fn focus_tab_index(&mut self, index: usize) {
        if index < DOCK_TOOLS.len() {
            self.focused_tab_index = Some(index);
        }
    }

    pub fn focused_tab_index(&self) -> Option<usize> {
        self.focused_tab_index
    }

    pub fn chrome(&self) -> DockChromeProjection {
        let active = self.active_tool();
        let tabs = DOCK_TOOLS
            .iter()
            .enumerate()
            .map(|(index, tool)| {
                let unavailable = self.tool_availability(*tool).is_err();
                let mut accessibility =
                    AccessibilityMetadata::new(AccessibleRole::Tab, tool.label())
                        .expect("dock tool name");
                accessibility.set_disabled(unavailable);
                accessibility.set_focused(self.focused_tab_index == Some(index));
                DockTabProjection {
                    tool: *tool,
                    name: tool.label().to_string(),
                    selected: *tool == active,
                    unavailable,
                    shortcut: DockShortcut::AltTool((index + 1) as u8),
                    accessibility,
                }
            })
            .collect();
        let tab_list = AccessibilityMetadata::new(AccessibleRole::TabList, "Context dock tools")
            .expect("tab list name");
        let resize_handle = (!self.is_collapsed()).then(|| DockResizeHandleProjection {
            accessibility: AccessibilityMetadata::new(AccessibleRole::Button, "Resize dock")
                .expect("resize name"),
            size_ratio: self.size_ratio(),
        });
        DockChromeProjection {
            edge: self.edge,
            collapsed: self.is_collapsed(),
            tabs,
            tab_list,
            active_tool: active,
            unavailable: self.tool_availability(active).err(),
            resize_handle,
        }
    }

    pub fn handle_gpui_pointer(&mut self, phase: PointerPhase, press: PointerPress) -> bool {
        match phase {
            PointerPhase::Down => self.pointer_down(press),
            PointerPhase::Move => self.pointer_move(press),
            PointerPhase::Up => self.pointer_up(press),
            PointerPhase::Cancel => self.pointer_cancel(press),
        }
    }

    pub fn terminal_pane_model(&self) -> TerminalPaneModel {
        let memory = self.current_memory();
        terminal_pane_from_replica(ReplicaPaneRequest {
            active_project: "",
            session_label: "task terminal",
            replica_view: memory.replica_view.as_ref(),
            last_valid_view: memory.last_valid_view.as_ref(),
            overlay: overlay_from(memory.surface_state, memory.exit_summary.as_deref()),
            selection: memory.viewport.selection,
            search: memory.viewport.search.clone(),
            search_highlight: memory.viewport.search_highlight,
            scrollbar: memory.viewport.scrollbar,
        })
    }

    pub fn render_context_dock(&self, tokens: ThemeTokens) -> impl IntoElement {
        let chrome = self.chrome();
        let tabs = chrome.tabs.into_iter().fold(
            div().flex().bg(rgb(tokens.surfaces.raised.to_u32())),
            |row, tab| {
                row.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_color(rgb(tokens.text.primary.to_u32()))
                        .child(tab.name),
                )
            },
        );
        let body = if self.showing_raw_terminal() && !self.is_collapsed() {
            render_terminal_surface(&self.terminal_pane_model(), None).into_any_element()
        } else {
            let unavailable = chrome
                .unavailable
                .as_ref()
                .map(|reason| match reason.reason {
                    DockUnavailableReason::NoTaskSelected => "Select a task to use this dock",
                    DockUnavailableReason::MissingHostProjection => {
                        "This dock tool has no host projection yet"
                    }
                    DockUnavailableReason::NoMatchingTerminal => {
                        "No matching terminal replica is bound"
                    }
                })
                .unwrap_or("Dock");
            let empty = EmptyState::new(chrome.active_tool.label(), unavailable)
                .map(|state| state.rendered_payload())
                .unwrap_or_else(|_| unavailable.to_string());
            div()
                .text_color(rgb(tokens.text.muted.to_u32()))
                .child(empty)
                .into_any_element()
        };
        let resize = if chrome.resize_handle.is_some() {
            div()
                .h(gpui::px(4.0))
                .bg(rgb(tokens.surfaces.raised.to_u32()))
                .into_any_element()
        } else {
            div().into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .bg(rgb(tokens.surfaces.sunken.to_u32()))
            .child(tabs)
            .child(resize)
            .child(body)
    }

    fn capture_press(&mut self, task_id: TaskId, press: PointerPress) {
        let identity = self.current_memory().identity;
        self.press_owner = Some(DockPressOwner {
            task_id,
            agent_session_id: identity.map(|identity| identity.agent_session_id),
            resource_id: identity.map(|identity| identity.resource_id),
            runtime_generation: identity.map(|identity| identity.runtime_generation),
            resource_generation: identity.map(|identity| identity.resource_generation),
            focus_epoch: self.focus.current(),
            pointer_id: press.pointer_id,
            button: press.button,
            surface: press.surface,
            tool: self.active_tool(),
            action_epoch: self.action_epoch,
        });
        self.terminal_mouse_report_emitted = false;
        self.terminal_selection_changed = false;
    }

    fn owner_matches_current(&self, owner: DockPressOwner) -> bool {
        let identity = self.current_memory().identity;
        self.selected_task == Some(owner.task_id)
            && owner.focus_epoch == self.focus.current()
            && owner.action_epoch == self.action_epoch
            && owner.agent_session_id == identity.map(|identity| identity.agent_session_id)
            && owner.resource_id == identity.map(|identity| identity.resource_id)
            && owner.runtime_generation == identity.map(|identity| identity.runtime_generation)
            && owner.resource_generation == identity.map(|identity| identity.resource_generation)
    }

    fn press_fields_match(owner: DockPressOwner, press: PointerPress) -> bool {
        owner.pointer_id == press.pointer_id
            && owner.button == press.button
            && owner.surface == press.surface
    }

    fn mark_needs_resync(&mut self) {
        self.needs_resync = true;
        self.press_owner = None;
        self.terminal_click_completed = false;
        self.terminal_mouse_report_emitted = false;
    }

    fn advance_epochs(&mut self) {
        let _ = self.focus.advance();
        self.action_epoch.sequence = self.action_epoch.sequence.saturating_add(1);
    }

    fn current_memory(&self) -> RememberedDockState {
        self.selected_task
            .and_then(|task_id| self.remembered.get(&task_id).cloned())
            .unwrap_or_else(RememberedDockState::default_state)
    }

    fn with_memory(&mut self, update: impl FnOnce(&mut RememberedDockState)) {
        let Some(task_id) = self.selected_task else {
            return;
        };
        self.remember_task(task_id);
        if let Some(memory) = self.remembered.get_mut(&task_id) {
            update(memory);
        }
    }

    fn remember_task(&mut self, task_id: TaskId) {
        if self.remembered.contains_key(&task_id) {
            return;
        }
        while self.remembered_order.len() >= MAX_REMEMBERED_TASKS {
            let evict_at = self
                .remembered_order
                .iter()
                .position(|candidate| Some(*candidate) != self.selected_task);
            let Some(evict_at) = evict_at else {
                break;
            };
            let evicted = self.remembered_order.remove(evict_at);
            self.remembered.remove(&evicted);
        }
        self.remembered
            .insert(task_id, RememberedDockState::default_state());
        self.remembered_order.push(task_id);
    }
}

fn snapshot_census(
    census: Option<&ProcessManagerCensus<'_>>,
) -> Result<RuntimeCensusSnapshot, DependencyUnavailable> {
    let Some(census) = census else {
        return Err(DependencyUnavailable::RuntimeCensus);
    };
    let before_roots = census.provider_root_count();
    let before_readers = census.pty_reader_count();
    Ok(RuntimeCensusSnapshot {
        provider_roots_before: before_roots,
        provider_roots_after: census.provider_root_count(),
        pty_readers_before: before_readers,
        pty_readers_after: census.pty_reader_count(),
    })
}

fn overlay_from(state: TerminalSurfaceState, summary: Option<&str>) -> TerminalReplicaOverlay {
    match state {
        TerminalSurfaceState::Live => TerminalReplicaOverlay::None,
        TerminalSurfaceState::Reconnecting => TerminalReplicaOverlay::Reconnecting,
        TerminalSurfaceState::Resyncing => TerminalReplicaOverlay::Resyncing,
        TerminalSurfaceState::Exited => TerminalReplicaOverlay::Exited {
            summary: summary.unwrap_or("Terminal exited").to_string(),
        },
    }
}

fn bound_search_query(value: &str) -> String {
    if value.trim().is_empty() {
        return String::new();
    }
    redacted_bounded_text(
        "dock search",
        value,
        MAX_SEARCH_SCALARS,
        MAX_SEARCH_SCALARS * 4,
    )
    .unwrap_or_default()
}

fn bound_exit_summary(value: &str) -> String {
    if value.trim().is_empty() {
        return String::new();
    }
    redacted_bounded_text(
        "dock exit summary",
        value,
        MAX_EXIT_SUMMARY_SCALARS,
        MAX_EXIT_SUMMARY_SCALARS * 4,
    )
    .unwrap_or_else(|_| String::from("Terminal exited"))
}

#[cfg(test)]
mod process_census_tests {
    use super::*;

    fn terminal_view() -> TerminalSessionView {
        use crate::state::SessionDimensions;
        use crate::terminal::session::TerminalScreenSnapshot;

        TerminalSessionView {
            runtime: crate::state::SessionRuntimeState::new(
                "task-terminal",
                std::path::PathBuf::from("."),
                SessionDimensions::default(),
                crate::terminal::session::TerminalBackend::default(),
            ),
            screen: TerminalScreenSnapshot::default(),
        }
    }

    fn census_client_model() -> (ClientModel, TaskId) {
        use crate::client::model::ClientModelBuilder;
        use crate::domain::{
            AgentRole, AgentSessionFacts, AgentSessionLifecycle, EnvironmentId, OwnerKind,
            ProjectId, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
            ReviewReadiness, SnapshotId, SnapshotItem, SnapshotPage, SnapshotSection, TaskActivity,
            TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts, TaskLifecycle,
            TaskSnapshotItem, WorkspaceRef,
        };

        let uuid = |tail: u8| {
            [
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, tail,
            ]
        };
        let task_id = TaskId::from_bytes(uuid(0x91)).expect("task");
        let agent_id = AgentSessionId::from_bytes(uuid(0x91)).expect("agent");
        let resource_id = ResourceId::from_bytes(uuid(0x91)).expect("resource");
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
                        title: "Census dock".into(),
                        description: None,
                        project_id: ProjectId::from_bytes(uuid(0x02)).expect("project"),
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        lifecycle: TaskLifecycle::Open,
                        action_epoch: 0,
                        revision: 1,
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

    #[test]
    fn view_switch_reads_process_manager_and_does_not_spawn() {
        let manager = ProcessManager::new();
        let census = ProcessManagerCensus::new(&manager);
        assert_eq!(census.provider_root_count(), 0);
        assert_eq!(census.pty_reader_count(), 0);
        let (model, task_id) = census_client_model();
        let mut dock = ContextDock::new(DockEdge::Right);
        dock.follow_task(task_id);
        dock.bind_from_model(&model).expect("bind");
        let report = dock
            .switch_to_raw_terminal(&model, Some(&census))
            .expect("switch");
        let snapshot = report.census().expect("count snapshot");
        assert!(snapshot.unchanged());
        assert_eq!(snapshot.provider_roots_before(), 0);
        assert_eq!(snapshot.pty_readers_before(), 0);
        assert_eq!(census.provider_root_count(), 0);
        assert_eq!(census.pty_reader_count(), 0);
        assert_eq!(
            census.one_provider_one_pty_proof(),
            Err(DependencyUnavailable::LiveRuntimeCensus)
        );
    }

    #[test]
    fn admitted_native_view_reaches_renderer_and_survives_overlay() {
        let (model, task_id) = census_client_model();
        let mut dock = ContextDock::new(DockEdge::Right);
        dock.follow_task(task_id);
        dock.bind_from_model(&model).expect("bind");

        let cursor = HostStreamCursor::full_snapshot(&model, task_id, 1).expect("cursor");
        dock.admit_host_view(cursor, terminal_view())
            .expect("admit native view");
        assert!(dock.replica_view().is_some());
        assert!(dock.terminal_pane_model().session.is_some());

        dock.present_host_overlay(&model, TerminalSurfaceState::Reconnecting, None)
            .expect("overlay");
        assert!(dock.terminal_pane_model().session.is_some());
        assert!(dock.terminal_pane_model().blocking_notice.is_some());
    }

    #[test]
    fn stale_generation_cannot_replace_native_view() {
        let (model, task_id) = census_client_model();
        let mut dock = ContextDock::new(DockEdge::Right);
        dock.follow_task(task_id);
        dock.bind_from_model(&model).expect("bind");
        let cursor = HostStreamCursor::full_snapshot(&model, task_id, 1).expect("cursor");
        dock.admit_host_view(cursor, terminal_view())
            .expect("admit native view");

        let mut stale = HostStreamCursor::full_snapshot(&model, task_id, 2).expect("cursor");
        stale.identity.runtime_generation = 2;
        assert!(matches!(
            dock.admit_host_view(stale, terminal_view()),
            Err(DockProjectionError::GenerationMismatch { .. })
        ));
        assert_eq!(
            dock.terminal_pane_model()
                .session
                .unwrap()
                .runtime
                .session_id,
            "task-terminal"
        );
    }
}
