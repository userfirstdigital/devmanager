//! The state-grouped task board (spec 2026-09-03 section 4). Pure model in
//! `model`, `age`, `activity`, `project_colour`, `layout`; the only gpui code
//! is `render`.

pub mod model;

pub use model::{
    board_state_of, build_board_model, group_of, BoardGroup, BoardGroupModel, BoardModel,
    BoardProgress, BoardRow, BoardState,
};
