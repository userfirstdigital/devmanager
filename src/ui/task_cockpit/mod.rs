//! Pure Task Cockpit projection contracts.

pub mod header;
pub mod inbox;

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
    Inbox, TaskList, TaskListOverflow, ViewportError, VirtualWindow, DEFAULT_VISIBLE_ROWS,
    FIXED_VIRTUAL_OVERSCAN, MAX_TASK_LIST_ITEMS,
};
