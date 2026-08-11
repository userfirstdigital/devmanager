//! Pure Task Cockpit projection contracts.

pub mod header;
pub mod inbox;

pub use header::{
    HeaderField, HeaderHighWaterLedger, HighWaterDecision, PendingHeaderActionOutcome,
    PendingHeaderActionQueue, ProjectedAction, SpecialistProjection, TaskHeaderModel, TaskIdentity,
    TopBarModel, TopBarProjectionController, TopBarProjectionInput,
};

pub use inbox::{
    Inbox, TaskList, TaskListOverflow, ViewportError, VirtualListViewport, VirtualWindow,
    DEFAULT_VISIBLE_ROWS, FIXED_VIRTUAL_OVERSCAN, MAX_TASK_LIST_ITEMS, MAX_VIRTUAL_SOURCE_ROWS,
};
