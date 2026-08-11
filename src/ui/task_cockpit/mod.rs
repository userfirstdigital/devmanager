//! Pure Task Cockpit projection contracts.

pub mod header;
pub mod inbox;
pub mod native;

pub use header::{
    ActionTarget, AgentIdentity, AgentProjection, AgentRoleProjection, ConnectObservation,
    ConnectObservationIdentity, ConnectState, CpuDiagnostic, CpuInputUnit, CpuProjection,
    HeaderAction, HeaderField, HeaderLayout, HostHealth, HostObservation, HostObservationIdentity,
    HostResourceObservation, HostResourceProjection, OverflowControl, PrimaryAgentProjection,
    ProjectProjection, ProjectedAction, QuotaObservation, QuotaObservationIdentity,
    QuotaProjection, RemoteHealth, RemoteObservation, RemoteObservationIdentity, StatusLink,
    TaskHeaderModel, TaskIdentity, TopBarAction, TopBarModel, TopBarProjectionController,
    TopBarProjectionError, TopBarProjectionInput, TopBarStatus, TopBarStatusLink, TurnProjection,
    UpdateObservation, UpdateObservationIdentity, UpdateState, WorkspaceProjection,
    MAX_HEADER_SPECIALISTS, MAX_TOP_BAR_QUOTAS, MAX_TOP_BAR_QUOTA_CACHE, NARROW_HEADER_WIDTH_PX,
    PROVIDER_QUOTA_MAX_AGE_MS, STANDARD_HEADER_WIDTH_PX,
};

pub use inbox::{
    Inbox, TaskList, TaskListOverflow, ViewportError, VirtualWindow, DEFAULT_VISIBLE_ROWS,
    FIXED_VIRTUAL_OVERSCAN, MAX_TASK_LIST_ITEMS,
};

pub use native::{
    bind_native_next_actions, is_task_details_action, native_next_host_channel, HostSnapshot,
    HostSnapshotError, NativeNextDispatchStatus, NativeNextHeaderMenu, NativeNextHeaderMenuItem,
    NativeNextHostAttachment, NativeNextHostClient, NativeNextHostCommand, NativeNextHostEvent,
    NativeNextHostReceipt, NativeNextHostState, NativeNextHostTick, NativeNextHostTickAdapter,
    NativeNextHostWorker, NativeNextRenderNode, NativeNextRenderTree, NativeNextTaskCockpit,
    NativeNextTaskCockpitProjection, NativeNextTaskCockpitSurface, NativeNextTopBarLayout,
    NativeNextUnavailable, NATIVE_NEXT_ACTION_DRAIN_LIMIT, NATIVE_NEXT_ACTION_QUEUE_CAPACITY,
    NATIVE_NEXT_HOST_CHANNEL_CAPACITY, NATIVE_NEXT_HOST_EVENT_DRAIN_LIMIT,
};
