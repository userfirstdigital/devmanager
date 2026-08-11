//! Pure Task Cockpit projection contracts.

pub mod inbox;

pub use inbox::{
    Inbox, TaskList, TaskListOverflow, ViewportError, VirtualKeysetWindow, VirtualListViewport,
    VirtualWindow, DEFAULT_VISIBLE_ROWS, FIXED_VIRTUAL_OVERSCAN, MAX_VIRTUAL_WINDOW_ROWS,
};
