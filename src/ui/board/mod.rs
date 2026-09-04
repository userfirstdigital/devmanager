//! The state-grouped task board (spec 2026-09-03 section 4). Pure model in
//! `model`, `age`, `activity`, `project_colour`, `layout`; the only gpui code
//! is `render`.

pub mod activity;
pub mod age;
pub mod model;
pub mod project_colour;

pub use activity::{board_activity, BoardActivity, DOING_NOW_MAX_CHARS};
pub use age::{format_age, StateClock};
pub use model::{
    board_state_of, build_board_model, group_of, BoardGroup, BoardGroupModel, BoardModel,
    BoardProgress, BoardRow, BoardState,
};
pub use project_colour::{ProjectColourBook, PROJECT_PALETTE};
