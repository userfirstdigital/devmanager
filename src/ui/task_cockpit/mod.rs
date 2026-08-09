//! Pure Task Cockpit projection contracts.

pub mod inbox;

pub use inbox::{
    Inbox, TaskList, TaskListOverflow, ViewportError, VirtualWindow, DEFAULT_VISIBLE_ROWS,
    FIXED_VIRTUAL_OVERSCAN, MAX_TASK_LIST_ITEMS,
};
