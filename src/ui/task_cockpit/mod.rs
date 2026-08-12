//! Pure Task Cockpit projection contracts.

mod bootstrap;
pub mod inbox;

pub use bootstrap::NativeNextTaskCockpit;
pub use inbox::{
    render_native_inbox, render_native_inbox_with_actions, Inbox, InboxActionEpochs, InboxError,
    InboxFilter, InboxItemKey, InboxOverflow, InboxPresentationWidth, InboxRenderItem,
    InboxRenderModel, InboxRenderRow, InboxRowActionCapture, InboxRowMouseDownHandler,
    InboxRuntime, InboxSection, InboxState, LiveClientSubscription, PrimaryProviderIcon,
    PrimaryProviderState, ResourceSummary, RuntimeSummary, SearchProgress, SearchWorkerState,
    TaskRow, TaskRowDisplay, TaskRowModel, UnreadCursor, ViewportError, VirtualWindow,
    DEFAULT_VISIBLE_ROWS, FIXED_VIRTUAL_OVERSCAN, MAX_ACCESSIBLE_DESCRIPTION_CHARS,
    MAX_ACCESSIBLE_NAME_CHARS, MAX_PROJECT_LABEL_CHARS, MAX_PROVIDER_LABEL_CHARS, MAX_SEARCH_CHARS,
    MAX_SECONDARY_LABEL_CHARS, MAX_TASK_LIST_ITEMS, MAX_WORKTREE_LABEL_CHARS,
};
