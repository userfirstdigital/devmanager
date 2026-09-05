//! The one-line top bar across the whole window (spec 2026-09-03 section 3,
//! composition A). The board column is the board and nothing else, so the
//! brand, the project scope, the keyboard hints and the settings glyph live
//! here instead of wrapped around the board.
//!
//! Geometry and typography are copied from the approved mockup
//! `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/01-composition-A.html`
//! (`.dm-top`, `.dm-top .brand`, `.dm-top .kbd`); the numbers live in
//! [`crate::ui::board::layout`] so they are asserted rather than remembered.

use std::rc::Rc;

use gpui::{
    div, px, AnyElement, App, FontWeight, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Styled, Window,
};

use crate::ui::actions::{KeyboardAction, KeyboardModel, KeyboardShortcut};
use crate::ui::board::layout::{
    KBD_FONT_SIZE, KBD_PADDING_X, KBD_PADDING_Y, KBD_RADIUS, NEEDS_YOU_CHIP_BORDER_ALPHA,
    TOP_BAR_BRAND_FONT_SIZE, TOP_BAR_CONNECTION_MAX_WIDTH, TOP_BAR_GAP, TOP_BAR_HEIGHT,
    TOP_BAR_PADDING_X, TOP_BAR_SCOPE_FONT_SIZE, TOP_BAR_SETTINGS_ICON_SIZE,
};
use crate::ui::board::model::{BoardGroup, BoardModel};
use crate::ui::tokens::ThemeTokens;

/// The product name, left-most in the bar. `&'static str` because it is not a
/// projection of anything: there is one application.
pub const BRAND: &str = "DevManager";

pub const SCOPE_ELEMENT_ID: &str = "native-top-bar-scope";
pub const NEEDS_YOU_ELEMENT_ID: &str = "native-top-bar-needs-you";
pub const CONNECTION_ELEMENT_ID: &str = "native-top-bar-connection";
pub const SETTINGS_ELEMENT_ID: &str = "native-top-bar-settings";

/// What the bar prints. Pure, so the needs-you count and the hint order are
/// testable without a window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopBarModel {
    pub brand: &'static str,
    pub scope_label: String,
    pub needs_you: usize,
    /// The host's own sentence about why it is not connected, or `None` while
    /// it is.
    ///
    /// This is the shell's only "host not connected" indicator. It lived in the
    /// 32 px workspace header, which composition A does not have, so it moved
    /// here rather than being deleted with the row that carried it (fix wave 1,
    /// F14). `Option`, not a `bool` plus a string: a connected host has no
    /// sentence, and two fields would let the painter print an empty chip.
    pub connection: Option<String>,
    /// `(keys, verb)` -- the chip's key text and the word after it. The keys
    /// are a `String` because they are read out of the keyboard model at build
    /// time, not written here.
    pub hints: Vec<(String, &'static str)>,
}

/// The three controls in the bar. The painter owns no state.
pub struct TopBarHandlers {
    pub on_scope: Rc<dyn Fn(&mut Window, &mut App)>,
    pub on_needs_you: Rc<dyn Fn(&mut Window, &mut App)>,
    pub on_settings: Rc<dyn Fn(&mut Window, &mut App)>,
}

/// The needs-you count is the Needs-you group's row count -- the same rows the
/// board paints under that heading, so the chip and the section cannot
/// disagree about how many things are waiting.
pub fn top_bar_model(
    scope_label: String,
    board: &BoardModel,
    keyboard: &KeyboardModel,
    connection: Option<String>,
) -> TopBarModel {
    TopBarModel {
        brand: BRAND,
        scope_label,
        needs_you: needs_you_count(board),
        // Blank is the same fact as absent, and a chip with no words in it
        // says "something is wrong" without saying what -- worse than no chip.
        connection: connection.filter(|headline| !headline.trim().is_empty()),
        hints: hints_from(keyboard),
    }
}

/// The three chips the composition prints, in its order, each naming the chord
/// [`KeyboardModel`] binds for it today rather than a key written beside it. A
/// chip whose action is unbound is dropped: a bar with two chips is better than
/// one promising a key that does nothing.
///
/// The mockup draws the macOS command glyph. The bindings are Ctrl chords and
/// every other shortcut label in this shell reads `Ctrl+`, so the labels follow
/// the model; only the modifier's spelling differs from the Mac reference, and
/// the chip geometry does not.
fn hints_from(keyboard: &KeyboardModel) -> Vec<(String, &'static str)> {
    [
        (
            keyboard
                .shortcut_for(KeyboardAction::OpenTaskSwitcher)
                .map(KeyboardShortcut::display_label),
            "switch",
        ),
        (
            keyboard
                .shortcut_for(KeyboardAction::ToggleZoom)
                .map(KeyboardShortcut::display_label),
            "zoom",
        ),
        (keyboard.focus_pane_label(), "focus"),
    ]
    .into_iter()
    .filter_map(|(keys, verb)| keys.map(|keys| (keys, verb)))
    .collect()
}

/// How many rows the amber chip counts: the Needs-you group's size, read off
/// the board model the board itself paints, so the chip, the section heading
/// and the accessibility node cannot disagree about how many things wait.
pub fn needs_you_count(board: &BoardModel) -> usize {
    board
        .groups
        .iter()
        .find(|group| group.group == BoardGroup::NeedsYou)
        .map(|group| group.rows.len())
        .unwrap_or(0)
}

/// `.dm-top .kbd`: a 1 px `borders.default` box, radius 4, 10.5 px muted text.
fn kbd_chip(id: &'static str, text: String, tokens: ThemeTokens) -> AnyElement {
    div()
        .id(id)
        .flex_none()
        .px(px(KBD_PADDING_X))
        .py(px(KBD_PADDING_Y))
        .rounded(px(KBD_RADIUS))
        .border_1()
        .border_color(tokens.borders.default.to_gpui())
        .text_size(px(KBD_FONT_SIZE))
        .text_color(tokens.text.muted.to_gpui())
        .child(text)
        .into_any_element()
}

pub fn top_bar_element(
    model: &TopBarModel,
    tokens: ThemeTokens,
    handlers: &TopBarHandlers,
) -> AnyElement {
    let on_scope = handlers.on_scope.clone();
    let on_needs_you = handlers.on_needs_you.clone();
    let on_settings = handlers.on_settings.clone();
    let needs_you = model.needs_you;
    let mut bar = div()
        .id("native-top-bar")
        .w_full()
        .h(px(TOP_BAR_HEIGHT))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(TOP_BAR_GAP))
        .px(px(TOP_BAR_PADDING_X))
        .bg(tokens.surfaces.canvas.to_gpui())
        .border_b(px(1.0))
        .border_color(tokens.borders.subtle.to_gpui())
        .child(
            div()
                .flex_none()
                .text_size(px(TOP_BAR_BRAND_FONT_SIZE))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tokens.text.primary.to_gpui())
                .child(model.brand),
        )
        .child(
            div()
                .id(SCOPE_ELEMENT_ID)
                .tab_stop(true)
                .flex_none()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(TOP_BAR_SCOPE_FONT_SIZE))
                .text_color(tokens.text.secondary.to_gpui())
                .cursor_pointer()
                .hover(|style| style.text_color(tokens.text.primary.to_gpui()))
                .on_mouse_down(
                    MouseButton::Left,
                    move |_event: &MouseDownEvent, window, app| {
                        (on_scope)(window, app);
                    },
                )
                .child(model.scope_label.clone()),
        )
        // The spacer, not `justify_between`: the hints must sit hard right
        // however many chips there are, and the brand and scope must stay
        // adjacent on the left.
        .child(div().flex_1().min_w(px(0.0)));
    // Left of the needs-you chip: a host that is not connected outranks the
    // count of tasks waiting on a person, because none of them can move until
    // it is back.
    if let Some(headline) = model.connection.clone() {
        bar = bar.child(
            div()
                .id(CONNECTION_ELEMENT_ID)
                .flex_none()
                .max_w(px(TOP_BAR_CONNECTION_MAX_WIDTH))
                .overflow_hidden()
                .px(px(KBD_PADDING_X))
                .py(px(KBD_PADDING_Y))
                .rounded(px(KBD_RADIUS))
                .border_1()
                // The amber chip's construction, in the red half of the
                // vocabulary: the state colour for the text and the same
                // colour at the same low alpha for the rule. Sharing the alpha
                // rather than solving a second one keeps the two chips looking
                // like one control in two states.
                .border_color(
                    tokens
                        .status
                        .destructive
                        .with_alpha(NEEDS_YOU_CHIP_BORDER_ALPHA)
                        .to_gpui(),
                )
                .text_size(px(KBD_FONT_SIZE))
                .text_color(tokens.status.destructive.to_gpui())
                // The headline is three segments long; it ellipses inside the
                // chip's ceiling rather than pushing the bar's right-hand end
                // off the window. `w_full` on the inner child is what gives
                // GPUI the definite width an ellipsis is measured against.
                .child(div().w_full().truncate().child(headline)),
        );
    }
    if needs_you > 0 {
        bar = bar.child(
            div()
                .id(NEEDS_YOU_ELEMENT_ID)
                .tab_stop(true)
                .flex_none()
                .px(px(KBD_PADDING_X))
                .py(px(KBD_PADDING_Y))
                .rounded(px(KBD_RADIUS))
                .border_1()
                .border_color(
                    tokens
                        .status
                        .attention
                        .with_alpha(NEEDS_YOU_CHIP_BORDER_ALPHA)
                        .to_gpui(),
                )
                .text_size(px(KBD_FONT_SIZE))
                .text_color(tokens.status.attention.to_gpui())
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    move |_event: &MouseDownEvent, window, app| {
                        (on_needs_you)(window, app);
                    },
                )
                .child(format!("{needs_you} need you")),
        );
    }
    for (index, (keys, verb)) in model.hints.iter().enumerate() {
        let id = match index {
            0 => "native-top-bar-hint-0",
            1 => "native-top-bar-hint-1",
            _ => "native-top-bar-hint-2",
        };
        bar = bar.child(kbd_chip(id, format!("{keys} {verb}"), tokens));
    }
    bar.child(
        div()
            .id(SETTINGS_ELEMENT_ID)
            .tab_stop(true)
            .flex_none()
            .flex()
            .items_center()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                move |_event: &MouseDownEvent, window, app| {
                    (on_settings)(window, app);
                },
            )
            .child(crate::icons::app_icon(
                crate::icons::SETTINGS,
                TOP_BAR_SETTINGS_ICON_SIZE,
                tokens.text.muted.to_u32(),
            )),
    )
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{HostId, HostTaskKey};
    use crate::domain::id::TaskId;
    use crate::ui::board::model::{build_board_model, BoardRow, BoardState};
    use crate::ui::task_cockpit::inbox::PrimaryProviderIcon;

    fn row(state: BoardState) -> BoardRow {
        BoardRow {
            key: HostTaskKey::new(HostId::LocalProfile("p".into()), TaskId::new()),
            title: "t".into(),
            state,
            why: state.why_label().to_string(),
            state_age_ms: 0,
            progress: None,
            provider: PrimaryProviderIcon::Claude,
            project_colour: 0,
            project_id: None,
            project_label: "p".into(),
            branch: "main".into(),
            last_activity_ms: 0,
            open: None,
            active: false,
        }
    }

    #[test]
    fn needs_you_chip_appears_only_when_a_needs_you_row_exists() {
        let keyboard = KeyboardModel::default();
        let empty = build_board_model(vec![], false);
        assert_eq!(
            top_bar_model("All projects".into(), &empty, &keyboard, None).needs_you,
            0
        );
        let one = build_board_model(vec![row(BoardState::Question)], false);
        assert_eq!(
            top_bar_model("All projects".into(), &one, &keyboard, None).needs_you,
            1
        );
    }

    /// A Working row is not a needs-you row, so the chip must not count it.
    ///
    /// The discriminating fixture is the one with NO needs-you rows: empty
    /// groups are dropped from the model, so there the first group is Working
    /// and "count the first group" answers 2 while the truth is 0. The mixed
    /// fixture below cannot catch that shortcut on its own -- with a Blocked
    /// row present the first group IS Needs-you and both answers are 1 -- so
    /// it only pins that Blocked counts.
    #[test]
    fn the_count_is_the_needs_you_group_not_the_first_group() {
        let keyboard = KeyboardModel::default();
        let none = build_board_model(
            vec![row(BoardState::Working), row(BoardState::Working)],
            false,
        );
        assert_eq!(
            none.groups[0].group,
            BoardGroup::Working,
            "the fixture's first group must not be Needs-you, or this proves nothing"
        );
        assert_eq!(none.groups[0].rows.len(), 2, "the naive answer here is 2");
        assert_eq!(
            top_bar_model("All projects".into(), &none, &keyboard, None).needs_you,
            0
        );
        let mixed = build_board_model(
            vec![
                row(BoardState::Working),
                row(BoardState::Working),
                row(BoardState::Blocked),
            ],
            false,
        );
        assert_eq!(
            top_bar_model("All projects".into(), &mixed, &keyboard, None).needs_you,
            1,
            "Blocked is a needs-you state"
        );
    }

    #[test]
    fn hints_are_the_three_composition_chips_in_order() {
        let m = top_bar_model(
            "All projects".into(),
            &build_board_model(vec![], false),
            &KeyboardModel::default(),
            None,
        );
        let verbs: Vec<_> = m.hints.iter().map(|(_, v)| *v).collect();
        assert_eq!(verbs, vec!["switch", "zoom", "focus"]);
    }

    /// The chips name the chords the model actually binds. Ctrl+K is the
    /// palette and Ctrl+P the switcher, so a chip table written beside the
    /// model rather than read out of it prints a key that does not switch
    /// anything -- and nothing fails when a rebinding moves the chord.
    #[test]
    fn the_hint_chips_are_the_models_own_bindings() {
        let keyboard = KeyboardModel::default();
        let m = top_bar_model(
            "All projects".into(),
            &build_board_model(vec![], false),
            &keyboard,
            None,
        );
        let switcher = keyboard
            .shortcut_for(KeyboardAction::OpenTaskSwitcher)
            .expect("the model binds the task switcher")
            .display_label();
        assert_eq!(m.hints[0].0, switcher.as_str(), "switch chip");
        let palette = keyboard
            .shortcut_for(KeyboardAction::OpenPalette)
            .expect("the model binds the palette")
            .display_label();
        assert_ne!(
            m.hints[0].0,
            palette.as_str(),
            "the switch chip must not name the palette's chord"
        );
        let zoom = keyboard
            .shortcut_for(KeyboardAction::ToggleZoom)
            .expect("the model binds zoom")
            .display_label();
        assert_eq!(m.hints[1].0, zoom.as_str(), "zoom chip");
        let focus = keyboard
            .focus_pane_label()
            .expect("the model binds directional focus");
        assert_eq!(m.hints[2].0, focus.as_str(), "focus chip");
    }

    #[test]
    fn the_scope_label_is_carried_through_verbatim() {
        let m = top_bar_model(
            "Snake Game".into(),
            &build_board_model(vec![], false),
            &KeyboardModel::default(),
            None,
        );
        assert_eq!(m.scope_label, "Snake Game");
        assert_eq!(m.brand, "DevManager");
    }

    /// F14: the shell's only "host not connected" indicator. It lived in the
    /// 32 px workspace header that composition A does not have, so it moved
    /// into the bar rather than being deleted with the row around it.
    ///
    /// Both directions, because a chip that is always absent and a chip that
    /// is always present are the same defect with the sign flipped: absent
    /// while the host is connected, present and carrying the host's own
    /// headline verbatim while it is not.
    #[test]
    fn the_connection_chip_appears_only_while_the_host_is_not_connected() {
        let board = build_board_model(vec![], false);
        let keyboard = KeyboardModel::default();

        let connected = top_bar_model("All projects".into(), &board, &keyboard, None);
        assert_eq!(
            connected.connection, None,
            "a connected host has nothing to say and gets no chip"
        );

        let headline = "Disconnected · retrying in 4s · build 1.2.3";
        let offline = top_bar_model(
            "All projects".into(),
            &board,
            &keyboard,
            Some(headline.to_string()),
        );
        assert_eq!(
            offline.connection.as_deref(),
            Some(headline),
            "the chip carries the host's own headline verbatim, not a copy of the words"
        );

        // Blank is the same fact as absent. A chip with no words in it says
        // "something is wrong" without saying what, which is worse than none.
        for blank in ["", "   "] {
            assert_eq!(
                top_bar_model(
                    "All projects".into(),
                    &board,
                    &keyboard,
                    Some(blank.to_string())
                )
                .connection,
                None,
                "a blank headline must not paint an empty chip"
            );
        }
    }

    /// The chip is painted left of the needs-you chip, in the destructive half
    /// of the vocabulary, built the same way the amber one is. A source scan
    /// because the order is a position in the builder chain and the colours
    /// are tokens: neither is readable out of the model.
    #[test]
    fn the_connection_chip_is_the_amber_chips_construction_in_red_and_sits_left_of_it() {
        let source = include_str!("topbar.rs");
        let painter = source
            .split("#[cfg(test)]")
            .next()
            .expect("the painter is everything above its tests");
        let connection = painter
            .find("CONNECTION_ELEMENT_ID)")
            .expect("the bar paints a connection chip");
        let needs_you = painter
            .find("NEEDS_YOU_ELEMENT_ID)")
            .expect("the bar paints a needs-you chip");
        assert!(
            connection < needs_you,
            "the connection chip is painted before the needs-you chip, so it sits to its left"
        );

        let chip = &painter[connection..needs_you];
        assert!(
            chip.contains("tokens.status.destructive.to_gpui()"),
            "the chip's text is status.destructive"
        );
        assert!(
            chip.contains("NEEDS_YOU_CHIP_BORDER_ALPHA"),
            "the rule is the state colour at the same low alpha the amber chip uses"
        );
        assert!(
            chip.contains("TOP_BAR_CONNECTION_MAX_WIDTH"),
            "the chip is bounded, or a long headline pushes the bar's right end off the window"
        );
        let compact: String = chip.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            compact.contains(".w_full().truncate()"),
            "the headline must resolve its width through an inner w_full child to ellipse"
        );
    }
}
