//! The task panel's chrome (spec 2026-09-03 section 6): the pure model in
//! `model`, the ⋯ menu and key vocabulary in `menu`, and the only gpui code in
//! `render`.
//!
//! The chrome is deliberately a projection of the board's [`BoardRow`], not a
//! second source of task state: the board and the panel say the same thing
//! about the same task, so a panel that disagreed with its board row would be
//! a bug nobody could attribute.
//!
//! [`BoardRow`]: crate::ui::board::BoardRow

pub mod model;

pub use model::{
    panel_chrome, status_layout, NeedsYou, PanelChrome, PanelStatus, PrimaryAction, StatusLayout,
    StatusTone, STATUS_CAUSE_MAX_CHARS,
};
