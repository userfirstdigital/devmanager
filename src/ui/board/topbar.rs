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
    MouseDownEvent, ParentElement, StatefulInteractiveElement, Styled, Window,
};

use crate::ui::board::layout::{
    KBD_FONT_SIZE, KBD_PADDING_X, KBD_PADDING_Y, KBD_RADIUS, NEEDS_YOU_CHIP_BORDER_ALPHA,
    TOP_BAR_BRAND_FONT_SIZE, TOP_BAR_GAP, TOP_BAR_HEIGHT, TOP_BAR_PADDING_X,
    TOP_BAR_SCOPE_FONT_SIZE, TOP_BAR_SETTINGS_ICON_SIZE,
};
use crate::ui::board::model::{BoardGroup, BoardModel};
use crate::ui::tokens::ThemeTokens;

/// The product name, left-most in the bar. `&'static str` because it is not a
/// projection of anything: there is one application.
pub const BRAND: &str = "DevManager";

/// The three hints the mockup prints, in the order it prints them.
///
/// The mockup draws the macOS command glyph; every other shortcut label in this
/// shell reads `Ctrl+`, and the bindings in [`crate::ui::actions::KeyboardModel`]
/// are Ctrl chords, so the keys named here are the keys that actually work. A
/// hint that names a key the platform does not honour is worse than a glyph
/// that differs from a Mac mockup.
pub const HINTS: [(&str, &str); 3] = [
    ("Ctrl+K", "switch"),
    ("Z", "zoom"),
    ("Ctrl+\u{2191}\u{2193}\u{2190}\u{2192}", "focus"),
];

pub const SCOPE_ELEMENT_ID: &str = "native-top-bar-scope";
pub const NEEDS_YOU_ELEMENT_ID: &str = "native-top-bar-needs-you";
pub const SETTINGS_ELEMENT_ID: &str = "native-top-bar-settings";

/// What the bar prints. Pure, so the needs-you count and the hint order are
/// testable without a window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopBarModel {
    pub brand: &'static str,
    pub scope_label: String,
    pub needs_you: usize,
    /// `(keys, verb)` — the chip's key text and the word after it.
    pub hints: Vec<(&'static str, &'static str)>,
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
pub fn top_bar_model(scope_label: String, board: &BoardModel) -> TopBarModel {
    let needs_you = board
        .groups
        .iter()
        .find(|group| group.group == BoardGroup::NeedsYou)
        .map(|group| group.rows.len())
        .unwrap_or(0);
    TopBarModel {
        brand: BRAND,
        scope_label,
        needs_you,
        hints: HINTS.to_vec(),
    }
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
        let empty = build_board_model(vec![], false);
        assert_eq!(top_bar_model("All projects".into(), &empty).needs_you, 0);
        let one = build_board_model(vec![row(BoardState::Question)], false);
        assert_eq!(top_bar_model("All projects".into(), &one).needs_you, 1);
    }

    /// A Working row is not a needs-you row, so the chip must not count it.
    /// Without this the "count the first group" shortcut passes on the empty
    /// board and lies on every populated one.
    #[test]
    fn the_count_is_the_needs_you_group_not_the_first_group() {
        let model = build_board_model(
            vec![
                row(BoardState::Working),
                row(BoardState::Working),
                row(BoardState::Blocked),
            ],
            false,
        );
        assert_eq!(top_bar_model("All projects".into(), &model).needs_you, 1);
    }

    #[test]
    fn hints_are_the_three_composition_chips_in_order() {
        let m = top_bar_model("All projects".into(), &build_board_model(vec![], false));
        let verbs: Vec<_> = m.hints.iter().map(|(_, v)| *v).collect();
        assert_eq!(verbs, vec!["switch", "zoom", "focus"]);
    }

    #[test]
    fn the_scope_label_is_carried_through_verbatim() {
        let m = top_bar_model("Snake Game".into(), &build_board_model(vec![], false));
        assert_eq!(m.scope_label, "Snake Game");
        assert_eq!(m.brand, "DevManager");
    }
}
