//! The board painter. Every colour comes from [`ThemeTokens`] or the project
//! palette; the amber and red states are the only saturated colours here.
//!
//! Geometry and typography are copied from the approved mockups
//! `03-board-rows-boxed-A.html` (chosen option A: full-width boxes, a rule above
//! and below each row, no side margins, the project stripe on the very left
//! edge), `04-project-stripe-1.html` (chosen option 1: a 3 px project-coloured
//! edge stripe) and `05-provider-mark-1.html` (chosen option 1: an 11 px grey
//! provider mark at the far right of the second line). The numbers live in
//! [`crate::ui::board::layout`] so they are asserted rather than remembered.
//!
//! Virtualisation: the board is a plain column. The spec caps the board at the
//! tasks one person can supervise and the fleet projection is already bounded
//! by `MAX_TASK_LIST_ITEMS`, so `uniform_list` would buy nothing and would cost
//! the shell's existing scroll container its single owner of scroll position.

use std::rc::Rc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, AnyElement, App, ElementId, FontWeight, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseUpEvent, ParentElement, StatefulInteractiveElement, Styled,
    Window,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::client::HostTaskKey;
use crate::ui::board::age::format_age;
use crate::ui::board::layout::{
    row_content_width, row_height, row_layout, BOARD_RAIL_WIDTH, BOARD_ROW_GAP, COUNT_FONT_SIZE,
    DONE_ROW_PADDING_BOTTOM, DONE_ROW_PADDING_TOP, DOT_CELL_GAP, DOT_CELL_WIDTH,
    GROUP_LABEL_FONT_SIZE, GROUP_LABEL_GAP, GROUP_LABEL_PADDING_BOTTOM, GROUP_LABEL_PADDING_TOP,
    HEADER_BUTTON_FONT_SIZE, HEADER_BUTTON_PADDING_X, HEADER_BUTTON_PADDING_Y,
    HEADER_BUTTON_RADIUS, HEADER_GAP, HEADER_PADDING_BOTTOM, HEADER_PADDING_TOP,
    HEADER_TITLE_FONT_SIZE, LINE_GAP, META_FONT_SIZE, META_GAP, META_LINE_HEIGHT,
    NEEDS_YOU_BORDER_ALPHA, PROVIDER_MARK_SIZE, RAIL_COUNT_FONT_SIZE, RAIL_DOT_COUNT_GAP,
    RAIL_DOT_SIZE, RAIL_GROUP_GAP, RAIL_PADDING_TOP, ROW_BORDER_WIDTH, ROW_COMPACT_PADDING_DELTA,
    ROW_PADDING_BOTTOM, ROW_PADDING_TOP, ROW_PADDING_X, ROW_STRIPE_WIDTH, SECOND_LINE_INDENT,
    SEGMENT_GAP, SEGMENT_HEIGHT, SEGMENT_RADIUS, SEGMENT_WIDTH, STATE_DOT_HALO_ALPHA,
    STATE_DOT_HALO_SIZE, STATE_DOT_SIZE, TITLE_FONT_SIZE, TITLE_LINE_HEIGHT,
};
use crate::ui::board::model::{BoardGroup, BoardModel, BoardProgress, BoardRow, BoardState};
use crate::ui::board::project_colour::ProjectColourBook;
use crate::ui::tokens::{Color, ThemeTokens};

/// What the shell does when a row is clicked, right-clicked or typed into.
/// The painter owns no state: it hands the row's key back and the shell decides.
///
/// Four handlers, matching the four the project rail's row carried. The two
/// capture-phase ones are not an optimisation: a pointer button the row does
/// not consume falls through to the terminal dock underneath, so a middle
/// click on a row would paste into the terminal. The painter only wires the
/// phases; which buttons are consumed is the shell's policy, because it is the
/// shell that owns the pointer grab being released.
pub struct BoardRowHandlers {
    pub on_capture_mouse_down: Rc<dyn Fn(&HostTaskKey, &MouseDownEvent, &mut Window, &mut App)>,
    pub on_capture_mouse_up: Rc<dyn Fn(&HostTaskKey, &MouseUpEvent, &mut Window, &mut App)>,
    pub on_left_select: Rc<dyn Fn(&HostTaskKey, bool /*shift*/, &mut Window, &mut App)>,
    pub on_key_down: Rc<dyn Fn(&HostTaskKey, &KeyDownEvent, &mut Window, &mut App)>,
}

/// The three controls in the board header.
pub struct BoardHeaderHandlers {
    pub on_new: Rc<dyn Fn(&mut Window, &mut App)>,
    pub on_menu: Rc<dyn Fn(&mut Window, &mut App)>,
    pub on_toggle_done: Rc<dyn Fn(&mut Window, &mut App)>,
}

/// Hash host + task into a UUID element identity so the same raw TaskId on two
/// hosts never shares a GPUI/accesskit node.
///
/// This is the single definition; `native_shell::stable_host_task_element_id`
/// forwards to it so the accessibility tree keeps the ids it already publishes.
pub fn board_row_element_id(key: &HostTaskKey) -> ElementId {
    let mut digest = Sha256::new();
    digest.update(b"native-host-task-element");
    digest.update([0]);
    digest.update(crate::ui::native_shell::host_identity_digest_bytes(
        &key.host,
    ));
    digest.update([0]);
    digest.update(key.task_id.as_bytes());
    let hash = digest.finalize();
    let mut uuid_bytes = [0_u8; 16];
    uuid_bytes.copy_from_slice(&hash[..16]);
    ElementId::Uuid(Uuid::from_bytes(uuid_bytes))
}

/// One element id per board section. The single definition: the painter puts
/// it on the element and `native_shell::board_group_element_id` forwards here
/// for the accessibility tree, so the two cannot name different sections.
pub fn board_group_element_id(group: BoardGroup) -> &'static str {
    match group {
        BoardGroup::NeedsYou => "board-group-needs-you",
        BoardGroup::Working => "board-group-working",
        BoardGroup::Idle => "board-group-idle",
        BoardGroup::Done => "board-group-done",
    }
}

/// The state colour and whether it carries a halo. Only the two states that
/// want a person are allowed to be saturated (spec 5.3).
fn state_paint(state: BoardState, tokens: ThemeTokens) -> (Color, bool) {
    match state {
        BoardState::Question | BoardState::Permission => (tokens.status.attention, true),
        BoardState::Blocked => (tokens.status.destructive, true),
        BoardState::Working => (tokens.text.muted, false),
        BoardState::Idle | BoardState::Done => (tokens.borders.strong, false),
    }
}

/// `.dot` is a 7 px disc inside the grid's 8 px first column; `.dot.you` adds a
/// 3 px shadow spread, i.e. a 13 px halo that does not move the title. The halo
/// is painted as an absolutely positioned sibling so it overflows the cell the
/// same way the CSS shadow does.
fn state_dot(row: &BoardRow, tokens: ThemeTokens) -> AnyElement {
    let (colour, halo) = state_paint(row.state, tokens);
    let dot_top = (STATE_DOT_HALO_SIZE - STATE_DOT_SIZE) / 2.0;
    let halo_left = (STATE_DOT_SIZE - STATE_DOT_HALO_SIZE) / 2.0;
    let mut cell = div()
        .relative()
        .flex_none()
        .w(px(DOT_CELL_WIDTH))
        .h(px(STATE_DOT_HALO_SIZE));
    if halo {
        cell = cell.child(
            div()
                .absolute()
                .left(px(halo_left))
                .top(px(0.0))
                .w(px(STATE_DOT_HALO_SIZE))
                .h(px(STATE_DOT_HALO_SIZE))
                .rounded_full()
                .bg(colour.with_alpha(STATE_DOT_HALO_ALPHA).to_gpui()),
        );
    }
    cell.child(
        div()
            .absolute()
            .left(px(0.0))
            .top(px(dot_top))
            .w(px(STATE_DOT_SIZE))
            .h(px(STATE_DOT_SIZE))
            .rounded_full()
            .bg(colour.to_gpui()),
    )
    .into_any_element()
}

/// The plan strip: one 9x4 segment per step, done behind, current lit, the rest
/// unlit. Exposed because the panel chrome paints the same strip in its title
/// row (plan 2), and two strips would drift.
pub fn segments_element(
    progress: BoardProgress,
    tokens: ThemeTokens,
    show_count: bool,
) -> AnyElement {
    let mut strip = div().flex().flex_none().items_center().gap(px(SEGMENT_GAP));
    for index in 0..progress.total {
        let colour = if index < progress.completed {
            tokens.text.secondary
        } else if index == progress.completed {
            tokens.text.primary
        } else {
            tokens.borders.default
        };
        strip = strip.child(
            div()
                .w(px(SEGMENT_WIDTH))
                .h(px(SEGMENT_HEIGHT))
                .rounded(px(SEGMENT_RADIUS))
                .bg(colour.to_gpui()),
        );
    }
    let mut wrap = div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(META_GAP))
        .child(strip);
    if show_count {
        wrap = wrap.child(
            div()
                .flex_none()
                .text_size(px(COUNT_FONT_SIZE))
                .text_color(tokens.text.muted.to_gpui())
                .child(format!("{}/{}", progress.completed, progress.total)),
        );
    }
    wrap.into_any_element()
}

/// The second line's left text. Idle rows say how long ago the last reply was
/// rather than repeating the word "Idle"; every other state carries the doing-now
/// text the shell already computed.
fn second_line_text(row: &BoardRow) -> String {
    match row.state {
        BoardState::Idle => format!("Last reply {}", format_age(row.state_age_ms)),
        _ => row.why.clone(),
    }
}

/// Project, branch and provider on one line, then the full untruncated title.
fn row_tooltip_text(row: &BoardRow) -> String {
    format!(
        "{} · {} · {}\n{}",
        row.project_label,
        row.branch,
        row.provider.label(),
        row.title
    )
}

pub fn board_row_element(
    row: &BoardRow,
    colours: &ProjectColourBook,
    tokens: ThemeTokens,
    width_px: f32,
    compact: bool,
    handlers: &BoardRowHandlers,
) -> AnyElement {
    // `width_px` is the COLUMN width. The breakpoints are a rule about the
    // space the meta line has, so the stripe and the two paddings come off it
    // first -- comparing them against the column leaves them unreachable,
    // because the column is clamped to 220 px at its narrowest.
    let layout = row_layout(row_content_width(width_px), row.progress);
    let one_line = row.state == BoardState::Done;
    let height = row_height(one_line, compact);
    let compact_delta = if compact && !one_line {
        ROW_COMPACT_PADDING_DELTA
    } else {
        0.0
    };
    let (padding_top, padding_bottom) = if one_line {
        (DONE_ROW_PADDING_TOP, DONE_ROW_PADDING_BOTTOM)
    } else {
        (
            ROW_PADDING_TOP - compact_delta,
            ROW_PADDING_BOTTOM - compact_delta,
        )
    };

    let stripe = colours.colour(row.project_colour);
    let (background, base_border) = if row.selected {
        (tokens.surfaces.selection, tokens.borders.strong)
    } else {
        (tokens.surfaces.raised, tokens.borders.subtle)
    };
    // The needs-you tint outranks the selection border: a row that wants a
    // person must not look calmer for being the one you happen to have open.
    let border = match row.state {
        BoardState::Question | BoardState::Permission => {
            tokens.status.attention.with_alpha(NEEDS_YOU_BORDER_ALPHA)
        }
        BoardState::Blocked => tokens.status.destructive.with_alpha(NEEDS_YOU_BORDER_ALPHA),
        _ => base_border,
    };
    // The reference PNG measures pure white on a row that asked a question and
    // the ordinary title colour on Working and on Blocked, so only the two
    // states waiting on an answer take `text.emphasis`. Blocked is loud in the
    // dot and the border, not in the title.
    let title_colour = if matches!(row.state, BoardState::Question | BoardState::Permission) {
        tokens.text.emphasis
    } else {
        tokens.text.primary
    };

    let tooltip_text = row_tooltip_text(row);
    let (capture_down_key, capture_up_key, select_key, key_key) = (
        row.key.clone(),
        row.key.clone(),
        row.key.clone(),
        row.key.clone(),
    );
    let (on_capture_down, on_capture_up, on_select, on_key) = (
        handlers.on_capture_mouse_down.clone(),
        handlers.on_capture_mouse_up.clone(),
        handlers.on_left_select.clone(),
        handlers.on_key_down.clone(),
    );

    let title_line = div()
        .flex()
        .items_center()
        .gap(px(DOT_CELL_GAP))
        .h(px(TITLE_LINE_HEIGHT))
        .child(state_dot(row, tokens))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(TITLE_FONT_SIZE))
                .line_height(px(TITLE_LINE_HEIGHT))
                .text_color(title_colour.to_gpui())
                .child(row.title.clone()),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(META_FONT_SIZE))
                .line_height(px(TITLE_LINE_HEIGHT))
                .text_color(tokens.text.muted.to_gpui())
                .child(format_age(row.state_age_ms)),
        );

    let meta_line = (!one_line).then(|| {
        div()
            .flex()
            .items_center()
            .gap(px(META_GAP))
            .h(px(META_LINE_HEIGHT))
            .pl(px(SECOND_LINE_INDENT))
            .text_size(px(META_FONT_SIZE))
            .line_height(px(META_LINE_HEIGHT))
            .text_color(tokens.text.muted.to_gpui())
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .child(second_line_text(row)),
            )
            .children(
                row.progress
                    .filter(|_| layout.show_segments)
                    .map(|progress| segments_element(progress, tokens, layout.show_count)),
            )
            .child(div().flex_none().child(crate::icons::app_icon(
                row.provider.glyph_path(),
                PROVIDER_MARK_SIZE,
                tokens.text.muted.to_u32(),
            )))
    });

    div()
        .id(board_row_element_id(&row.key))
        .tab_stop(true)
        .relative()
        .flex_none()
        .w_full()
        .h(px(height))
        .mb(px(BOARD_ROW_GAP))
        .bg(background.to_gpui())
        .border_t(px(ROW_BORDER_WIDTH))
        .border_b(px(ROW_BORDER_WIDTH))
        .border_color(border.to_gpui())
        .cursor_pointer()
        // A selected row keeps its selection fill under the pointer; hovering
        // it must not repaint it as an ordinary hovered row.
        .when(!row.selected, |row| {
            row.hover(|style| style.bg(tokens.surfaces.hover.to_gpui()))
        })
        .tooltip(move |window, app| {
            gpui_component::tooltip::Tooltip::new(tooltip_text.clone()).build(window, app)
        })
        // Capture first: the shell consumes the buttons that must not fall
        // through to the terminal dock, and releases the pointer grab it took.
        // Left is deliberately left to bubble so a nested affordance inside a
        // row can still receive its own mouse sequence.
        .capture_any_mouse_down(move |event: &MouseDownEvent, window, app| {
            (on_capture_down)(&capture_down_key, event, window, app);
        })
        .capture_any_mouse_up(move |event: &MouseUpEvent, window, app| {
            (on_capture_up)(&capture_up_key, event, window, app);
        })
        .on_mouse_down(
            MouseButton::Left,
            move |event: &MouseDownEvent, window, app| {
                (on_select)(&select_key, event.modifiers.shift, window, app);
            },
        )
        .on_key_down(move |event: &KeyDownEvent, window, app| {
            (on_key)(&key_key, event, window, app);
        })
        // The project stripe sits on the very left edge, full row height.
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(ROW_STRIPE_WIDTH))
                .bg(stripe.to_gpui()),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .h_full()
                .gap(px(LINE_GAP))
                .pt(px(padding_top))
                .pb(px(padding_bottom))
                .pl(px(ROW_PADDING_X))
                .pr(px(ROW_PADDING_X))
                .child(title_line)
                .children(meta_line),
        )
        .into_any_element()
}

/// The group's own colour in rail mode: the same meaning the state dot carries,
/// aggregated to the section.
fn group_colour(group: BoardGroup, tokens: ThemeTokens) -> Color {
    match group {
        BoardGroup::NeedsYou => tokens.status.attention,
        BoardGroup::Working => tokens.text.muted,
        // Idle and Done share the row's own quiet dot colour; `status.inactive`
        // is the same grey as `text.muted`, which would make Working and Done
        // indistinguishable on the rail.
        BoardGroup::Idle => tokens.borders.strong,
        BoardGroup::Done => tokens.borders.strong,
    }
}

fn header_element(tokens: ThemeTokens, handlers: &BoardHeaderHandlers) -> AnyElement {
    let on_new = handlers.on_new.clone();
    let on_menu = handlers.on_menu.clone();
    div()
        .flex()
        .items_center()
        .gap(px(HEADER_GAP))
        .pt(px(HEADER_PADDING_TOP))
        .pb(px(HEADER_PADDING_BOTTOM))
        .pl(px(ROW_PADDING_X))
        .pr(px(ROW_PADDING_X))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(HEADER_TITLE_FONT_SIZE))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tokens.text.primary.to_gpui())
                .child("Board"),
        )
        .child(
            div()
                .id("board-header-new")
                .tab_stop(true)
                .flex_none()
                .px(px(HEADER_BUTTON_PADDING_X))
                .py(px(HEADER_BUTTON_PADDING_Y))
                .rounded(px(HEADER_BUTTON_RADIUS))
                .border_1()
                .border_color(tokens.borders.strong.to_gpui())
                .text_size(px(HEADER_BUTTON_FONT_SIZE))
                .text_color(tokens.text.primary.to_gpui())
                .cursor_pointer()
                .hover(|style| style.bg(tokens.surfaces.hover.to_gpui()))
                .on_mouse_down(
                    MouseButton::Left,
                    move |_event: &MouseDownEvent, window, app| {
                        (on_new)(window, app);
                    },
                )
                .child("+ New"),
        )
        .child(
            div()
                .id("board-header-menu")
                .tab_stop(true)
                .flex_none()
                .text_size(px(HEADER_TITLE_FONT_SIZE))
                .text_color(tokens.text.muted.to_gpui())
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    move |_event: &MouseDownEvent, window, app| {
                        (on_menu)(window, app);
                    },
                )
                .child("\u{22ef}"),
        )
        .into_any_element()
}

/// The section label: the name, then the count, and for Done a disclosure that
/// toggles the section.
fn group_label_element(
    group: BoardGroup,
    count: usize,
    collapsed: bool,
    tokens: ThemeTokens,
    handlers: &BoardHeaderHandlers,
) -> AnyElement {
    let done = group == BoardGroup::Done;
    let label_colour = if done {
        tokens.text.muted
    } else {
        tokens.text.secondary
    };
    // One id per section: three labels sharing an ElementId would collide in
    // the accessibility tree and in GPUI's own element state.
    let element_id = board_group_element_id(group);
    let mut label = div()
        .id(element_id)
        .flex()
        .items_center()
        .gap(px(GROUP_LABEL_GAP))
        .pt(px(GROUP_LABEL_PADDING_TOP))
        .pb(px(GROUP_LABEL_PADDING_BOTTOM))
        .pl(px(ROW_PADDING_X))
        .pr(px(ROW_PADDING_X))
        .text_size(px(GROUP_LABEL_FONT_SIZE))
        .text_color(label_colour.to_gpui())
        .child(div().flex_none().child(group.label().to_uppercase()))
        .child(
            div()
                .flex_none()
                .text_color(tokens.text.muted.to_gpui())
                .child(count.to_string()),
        )
        .child(div().flex_1());
    if done {
        let on_toggle = handlers.on_toggle_done.clone();
        label = label
            .tab_stop(true)
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                move |_event: &MouseDownEvent, window, app| {
                    (on_toggle)(window, app);
                },
            )
            .child(
                div()
                    .flex_none()
                    .text_color(tokens.text.muted.to_gpui())
                    .child(if collapsed { "\u{25b8}" } else { "\u{25be}" }),
            );
    }
    label.into_any_element()
}

/// Rail mode: a 36 px column with one group dot per group in the group's
/// colour and the count beneath. No titles — the rail is a "is anything waiting
/// for me" glance, not a list.
fn rail_element(model: &BoardModel, tokens: ThemeTokens) -> AnyElement {
    let mut column = div()
        .w(px(BOARD_RAIL_WIDTH))
        .h_full()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(RAIL_GROUP_GAP))
        .pt(px(RAIL_PADDING_TOP))
        .bg(tokens.surfaces.canvas.to_gpui())
        .border_r(px(ROW_BORDER_WIDTH))
        .border_color(tokens.borders.subtle.to_gpui());
    for group in &model.groups {
        column = column.child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(RAIL_DOT_COUNT_GAP))
                .child(
                    div()
                        .w(px(RAIL_DOT_SIZE))
                        .h(px(RAIL_DOT_SIZE))
                        .rounded_full()
                        .bg(group_colour(group.group, tokens).to_gpui()),
                )
                .child(
                    div()
                        .text_size(px(RAIL_COUNT_FONT_SIZE))
                        .text_color(tokens.text.muted.to_gpui())
                        .child(group.rows.len().to_string()),
                ),
        );
    }
    column.into_any_element()
}

/// `body` replaces the sections under the header when the column has something
/// other than a task list to show: the shell's empty state when nothing is
/// live, and the archived browser when that view is open. Both need the header
/// above them -- it carries the `⋯` menu, which is the only pointer route back
/// out of the archived view and out of the rail.
pub fn render_board(
    model: &BoardModel,
    colours: &ProjectColourBook,
    tokens: ThemeTokens,
    width_px: f32,
    rail: bool,
    compact: bool,
    body: Option<AnyElement>,
    row_handlers: BoardRowHandlers,
    header_handlers: BoardHeaderHandlers,
) -> AnyElement {
    if rail {
        return rail_element(model, tokens);
    }
    let mut column = div()
        .w(px(width_px))
        .h_full()
        .flex()
        .flex_col()
        .bg(tokens.surfaces.canvas.to_gpui())
        .border_r(px(ROW_BORDER_WIDTH))
        .border_color(tokens.borders.subtle.to_gpui())
        .child(header_element(tokens, &header_handlers));
    if let Some(body) = body {
        return column.child(body).into_any_element();
    }
    for group in &model.groups {
        column = column.child(group_label_element(
            group.group,
            group.rows.len(),
            group.collapsed,
            tokens,
            &header_handlers,
        ));
        if group.collapsed {
            continue;
        }
        for row in &group.rows {
            column = column.child(board_row_element(
                row,
                colours,
                tokens,
                width_px,
                compact,
                &row_handlers,
            ));
        }
    }
    column.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::HostId;
    use crate::domain::id::TaskId;
    use crate::ui::board::model::{build_board_model, BoardProgress, BoardRow, BoardState};
    use crate::ui::task_cockpit::inbox::PrimaryProviderIcon;
    use crate::ui::tokens::{Density, Scale};

    fn sample_rows() -> Vec<BoardRow> {
        [
            BoardState::Question,
            BoardState::Permission,
            BoardState::Blocked,
            BoardState::Working,
            BoardState::Idle,
            BoardState::Done,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, state)| BoardRow {
            key: HostTaskKey::new(HostId::LocalProfile("p".into()), TaskId::new()),
            title: format!("row {index}"),
            state,
            why: state.why_label().into(),
            state_age_ms: 1_000 * index as i64,
            progress: (index % 2 == 0).then_some(BoardProgress {
                completed: 1,
                total: 3,
            }),
            provider: PrimaryProviderIcon::Claude,
            project_colour: index as u8,
            project_id: None,
            project_label: "p".into(),
            branch: "main".into(),
            last_activity_ms: 0,
            selected: index == 3,
        })
        .collect()
    }

    fn noop_row_handlers() -> BoardRowHandlers {
        BoardRowHandlers {
            on_capture_mouse_down: Rc::new(|_, _, _, _| {}),
            on_capture_mouse_up: Rc::new(|_, _, _, _| {}),
            on_left_select: Rc::new(|_, _, _, _| {}),
            on_key_down: Rc::new(|_, _, _, _| {}),
        }
    }

    fn noop_header_handlers() -> BoardHeaderHandlers {
        BoardHeaderHandlers {
            on_new: Rc::new(|_, _| {}),
            on_menu: Rc::new(|_, _| {}),
            on_toggle_done: Rc::new(|_, _| {}),
        }
    }

    /// The painter is the only board code that needs a GPUI app, so this is the
    /// one headless test: every state, both densities and both modes have to
    /// build an element tree without panicking.
    #[test]
    fn board_renders_every_state_without_panicking() {
        if crate::ui::native_shell::tests::rerun_headless_shell_test_in_child(
            "ui::board::render::tests::board_renders_every_state_without_panicking",
        ) {
            return;
        }
        let _guard = crate::ui::native_shell::tests::headless_shell_test_lock();
        gpui::Application::headless().run(|cx| {
            crate::ui::init(cx);
            let tokens = crate::ui::tokens::dark(Density::Comfortable, Scale::Scale100);
            let model = build_board_model(sample_rows(), true);
            let colours = ProjectColourBook::default();
            let _ = render_board(
                &model,
                &colours,
                tokens,
                crate::ui::board::layout::BOARD_COLUMN_WIDTH,
                false,
                false,
                None,
                noop_row_handlers(),
                noop_header_handlers(),
            );
            // Narrow enough to drop the count and then the strip, and compact.
            let _ = render_board(
                &model,
                &colours,
                tokens,
                150.0,
                false,
                true,
                None,
                noop_row_handlers(),
                noop_header_handlers(),
            );
            let collapsed = build_board_model(sample_rows(), false);
            let _ = render_board(
                &collapsed,
                &colours,
                tokens,
                BOARD_RAIL_WIDTH,
                true,
                true,
                None,
                noop_row_handlers(),
                noop_header_handlers(),
            );
            // A body replaces the sections and keeps the header: the empty
            // state and the archived browser both arrive this way, and an
            // empty model is exactly the case the empty state exists for.
            let empty = build_board_model(Vec::new(), false);
            assert!(!empty.has_rows());
            let _ = render_board(
                &empty,
                &colours,
                tokens,
                crate::ui::board::layout::BOARD_COLUMN_WIDTH,
                false,
                false,
                Some(
                    div()
                        .id("board-test-body")
                        .child("nothing here")
                        .into_any_element(),
                ),
                noop_row_handlers(),
                noop_header_handlers(),
            );
            cx.quit();
        });
    }

    /// A fixed UUIDv7, so the golden below is reproducible rather than a fresh
    /// random id each run. Same shape as `client::fleet`'s test helper.
    fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
        [
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, tail,
        ]
    }

    fn golden_key() -> HostTaskKey {
        HostTaskKey::new(
            HostId::LocalProfile("profile".into()),
            TaskId::from_bytes(fixed_uuid_v7(0x2a)).expect("fixed task id"),
        )
    }

    /// A golden, not a round trip. Asserting the painter against the shell would
    /// be `f(x) == f(x)` now that the shell forwards here, so this pins the
    /// literal id the pre-redesign `stable_host_task_element_id` produced. If it
    /// ever changes, every accessibility node on the board has been renamed and
    /// the tooling that addresses them by id has silently broken.
    #[test]
    fn row_element_id_is_the_pre_redesign_accessibility_identity() {
        assert_eq!(
            board_row_element_id(&golden_key()),
            ElementId::Uuid(Uuid::from_bytes([
                0x55, 0x24, 0x9a, 0x5f, 0x4f, 0xe5, 0x5a, 0x67, 0x85, 0x30, 0x9b, 0x90, 0xe6, 0xf3,
                0x83, 0xa2,
            ])),
            "board row element ids are a published contract, not an implementation detail"
        );
        let other = HostTaskKey::new(HostId::LocalProfile("other".into()), golden_key().task_id);
        assert_ne!(
            board_row_element_id(&golden_key()),
            board_row_element_id(&other),
            "the same task id on two hosts must not share a node"
        );
    }

    #[test]
    fn idle_rows_say_when_the_last_reply_was_rather_than_repeating_idle() {
        let mut row = sample_rows().remove(4);
        row.state = BoardState::Idle;
        row.state_age_ms = 18 * 60 * 1_000;
        assert_eq!(second_line_text(&row), "Last reply 18m");
        row.state = BoardState::Working;
        row.why = "cargo test".into();
        assert_eq!(second_line_text(&row), "cargo test");
    }

    #[test]
    fn the_tooltip_carries_what_the_row_had_to_truncate() {
        let mut row = sample_rows().remove(0);
        row.project_label = "devmanager".into();
        row.branch = "VisualDevManager".into();
        row.title = "a title far too long for a 236 px column".into();
        let text = row_tooltip_text(&row);
        assert!(text.starts_with("devmanager · VisualDevManager · Claude\n"));
        assert!(text.ends_with(&row.title));
    }

    /// The halo is the only place a token becomes translucent, and a dropped
    /// alpha would silently repaint the needs-you dot as a solid amber blob.
    #[test]
    fn needs_you_states_carry_a_halo_and_the_rest_do_not() {
        let tokens = crate::ui::tokens::dark(Density::Comfortable, Scale::Scale100);
        assert_eq!(
            state_paint(BoardState::Question, tokens),
            (tokens.status.attention, true)
        );
        assert_eq!(
            state_paint(BoardState::Permission, tokens),
            (tokens.status.attention, true)
        );
        assert_eq!(
            state_paint(BoardState::Blocked, tokens),
            (tokens.status.destructive, true)
        );
        assert!(!state_paint(BoardState::Working, tokens).1);
        assert!(!state_paint(BoardState::Idle, tokens).1);
        assert!(!state_paint(BoardState::Done, tokens).1);
    }
}
