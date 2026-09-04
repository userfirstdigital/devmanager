//! The state-grouped task board (spec 2026-09-03 section 4). Pure model in
//! `model`, `age`, `activity`, `project_colour`, `layout`; the gpui code is
//! `render` and `topbar`.

pub mod activity;
pub mod age;
pub mod layout;
pub mod model;
pub mod project_colour;
pub mod render;
pub mod topbar;

pub use activity::{board_activity, BoardActivity, DOING_NOW_MAX_CHARS};
pub use age::{format_age, StateClock};
pub use layout::{
    row_height, row_layout, BoardRowLayout, BOARD_COLUMN_WIDTH, BOARD_DONE_ROW_HEIGHT,
    BOARD_RAIL_WIDTH, BOARD_ROW_GAP, BOARD_ROW_HEIGHT, BOARD_ROW_HEIGHT_COMPACT, TOP_BAR_HEIGHT,
};
pub use model::{
    board_state_of, build_board_model, group_of, BoardGroup, BoardGroupModel, BoardModel,
    BoardProgress, BoardRow, BoardState,
};
pub use project_colour::{ProjectColourBook, PROJECT_PALETTE};
pub use render::{
    board_group_element_id, board_row_element, board_row_element_id, ordinal_chip, render_board,
    segments_element, BoardHeaderHandlers, BoardRowHandlers,
};
pub use topbar::{top_bar_element, top_bar_model, TopBarHandlers, TopBarModel};
