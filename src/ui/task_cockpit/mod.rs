//! Pure Task Cockpit projection contracts.

pub mod inbox;

pub use inbox::{
    Inbox, InboxError, InboxFilter, InboxItemKey, InboxPresentationWidth, InboxRenderItem,
    InboxRenderModel, InboxRenderRow, InboxSection, InboxState, PrimaryProviderState,
    ResourceSummary, RuntimeSummary, TaskList, TaskListOverflow, TaskRow, TaskRowDisplay,
    TaskRowModel, UnreadCursor, ViewportError, VirtualWindow, DEFAULT_VISIBLE_ROWS,
    FIXED_VIRTUAL_OVERSCAN, MAX_ACCESSIBLE_DESCRIPTION_CHARS, MAX_ACCESSIBLE_NAME_CHARS,
    MAX_PROJECT_LABEL_CHARS, MAX_PROVIDER_LABEL_CHARS, MAX_SECONDARY_LABEL_CHARS,
    MAX_TASK_LIST_ITEMS, MAX_WORKTREE_LABEL_CHARS,
};
