//! Pure Task Cockpit projection contracts.

pub mod header;
pub mod inbox;

pub use header::{
    ActionTarget, AgentIdentity, AgentProjection, AgentRoleProjection, ConnectObservation,
    ConnectObservationIdentity, ConnectState, CpuDiagnostic, CpuInputUnit, CpuProjection,
    HeaderAction, HeaderField, HeaderLayout, HostHealth, HostObservation, HostObservationIdentity,
    HostResourceObservation, HostResourceProjection, OverflowControl, PrimaryAgentProjection,
    ProjectProjection, ProjectedAction, QuotaObservation, QuotaObservationIdentity,
    QuotaProjection, StatusLink, TaskHeaderModel, TaskIdentity, TopBarAction, TopBarModel,
    TopBarProjectionInput, TopBarStatus, TopBarStatusLink, TurnProjection, UpdateObservation,
    UpdateObservationIdentity, UpdateState, WorkspaceProjection, MAX_HEADER_SPECIALISTS,
    MAX_TOP_BAR_QUOTAS, NARROW_HEADER_WIDTH_PX, PROVIDER_QUOTA_MAX_AGE_MS,
    STANDARD_HEADER_WIDTH_PX,
};

pub use inbox::{
    Inbox, TaskList, TaskListOverflow, ViewportError, VirtualWindow, DEFAULT_VISIBLE_ROWS,
    FIXED_VIRTUAL_OVERSCAN, MAX_TASK_LIST_ITEMS,
};
