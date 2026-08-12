//! Pure Task Cockpit projection contracts.

mod bootstrap;
pub mod inbox;

pub use bootstrap::NativeNextTaskCockpit;
pub mod header;
pub use header::{
    ActionTarget, AgentIdentity, AgentProjection, AgentResourceField, AgentRoleProjection,
    ConnectObservation, ConnectObservationIdentity, ConnectState, CpuDiagnostic, CpuInputUnit,
    CpuProjection, HeaderAction, HeaderField, HeaderFieldKey, HeaderHighWaterLedger, HeaderLayout,
    HighWaterDecision, HostHealth, HostObservation, HostObservationIdentity,
    HostResourceObservation, HostResourceProjection, OverflowControl, PendingHeaderActionError,
    PendingHeaderActionQueue, PrimaryAgentProjection, ProjectProjection, ProjectedAction,
    QuotaObservation, QuotaObservationIdentity, QuotaProjection, RemoteHealth, RemoteObservation,
    RemoteObservationIdentity, RemoteProjection, SpecialistWindow, StatusLink, TaskHeaderModel,
    TaskIdentity, TopBarAction, TopBarModel, TopBarProjectionController, TopBarProjectionError,
    TopBarProjectionInput, TopBarStatus, TopBarStatusLink, TurnProjection, UpdateObservation,
    UpdateObservationIdentity, UpdateState, WorkspaceProjection, HEADER_HIGH_WATER_TTL_MS,
    MAX_HEADER_HIGH_WATER_ENTRIES, MAX_HEADER_SPECIALISTS, MAX_PENDING_HEADER_ACTIONS,
    MAX_SPECIALIST_VIRTUAL_WINDOW, MAX_TOP_BAR_QUOTAS, MAX_TOP_BAR_QUOTA_CACHE,
    NARROW_HEADER_WIDTH_PX, PROVIDER_QUOTA_MAX_AGE_MS, STANDARD_HEADER_WIDTH_PX,
};

pub use inbox::{
    render_native_inbox, render_native_inbox_with_actions, Inbox, InboxActionEpochs, InboxError,
    InboxFilter, InboxItemKey, InboxOverflow, InboxPresentationWidth, InboxRenderItem,
    InboxRenderModel, InboxRenderRow, InboxRowActionCapture, InboxRowMouseDownHandler,
    InboxRuntime, InboxSection, InboxState, LiveClientSubscription, PrimaryProviderIcon,
    PrimaryProviderState, ResourceSummary, RuntimeSummary, SearchProgress, SearchWorkerState,
    TaskRow, TaskRowDisplay, TaskRowModel, UnreadCursor, ViewportError, VirtualWindow,
    TaskList, TaskListOverflow, VirtualKeysetWindow, VirtualListViewport, DEFAULT_VISIBLE_ROWS,
    FIXED_VIRTUAL_OVERSCAN, MAX_ACCESSIBLE_DESCRIPTION_CHARS,
    MAX_ACCESSIBLE_NAME_CHARS, MAX_PROJECT_LABEL_CHARS, MAX_PROVIDER_LABEL_CHARS, MAX_SEARCH_CHARS,
    MAX_SECONDARY_LABEL_CHARS, MAX_TASK_LIST_ITEMS, MAX_TASK_SOURCE_IDS, MAX_VIRTUAL_WINDOW_ROWS,
    MAX_WORKTREE_LABEL_CHARS,
};


/// Shared semantic renderer surfaces for task-cockpit messages.
 pub mod renderers;
 pub mod timeline;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
 pub enum NativeCockpitMount {
     HoldMissingShell,
 }

 pub const NATIVE_COCKPIT_MOUNT: NativeCockpitMount = NativeCockpitMount::HoldMissingShell;


 pub mod composer;
