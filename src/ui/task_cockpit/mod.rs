//! Pure Task Cockpit projection contracts.

pub mod inbox;

pub use inbox::{
    Inbox, InboxError, InboxFilter, InboxSection, InboxState, TaskList, TaskListOverflow, TaskRow,
    TaskRowModel, UnreadCursor, ViewportError, VirtualWindow, DEFAULT_VISIBLE_ROWS,
    FIXED_VIRTUAL_OVERSCAN, MAX_TASK_LIST_ITEMS,
};
