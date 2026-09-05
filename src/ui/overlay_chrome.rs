//! The one chrome vocabulary every overlay and secondary surface paints with.
//!
//! The redesign gives menus, the palette, the switcher, the pickers, the
//! settings pages and the empty states a single look (design-language rules 2,
//! 5, 6, 7, 9 and 12). Before this module each of those painters spelled the
//! look out again in place, so the twelve of them disagreed on radius, on row
//! padding, on which surface a selected row takes and on whether a shortcut is
//! rendered as a chip or as prose. The numbers live here once, and the colour
//! choices are pure functions so a test can assert them without a window.
//!
//! Geometry is read from the approved mockups
//! `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/01-composition-A.html`
//! (the `kbd` chip: 1 px border, radius 4, 1x6 padding, 10.5 px, muted) and
//! `02-panel-chrome-*.html`. Everything coloured comes from
//! [`crate::ui::tokens`].

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, AnyElement, Div, ElementId, InteractiveElement, IntoElement, ParentElement, Stateful,
    Styled,
};

use crate::ui::tokens::{Color, ThemeTokens};

/// Radius 6 for menus and cards (rule 3).
pub const OVERLAY_RADIUS: f32 = 6.0;
/// Every overlay carries exactly one hairline (rule 7: `borders.default`).
pub const OVERLAY_BORDER_WIDTH: f32 = 1.0;
/// An anchored overlay hangs 4 px below the control that opened it (rule 7).
pub const OVERLAY_ANCHOR_DROP: f32 = 4.0;
/// Bounded height; past this the app scrollbar takes over (rules 7 and 8).
pub const OVERLAY_MAX_HEIGHT: f32 = 320.0;
/// The surface's own vertical inset. Rows are full-width, so there is no
/// horizontal inset to pair with it (rule 5: no side margins).
pub const OVERLAY_PADDING_Y: f32 = 4.0;

/// Rows: full-width, padding 5x10, radius 0 (rules 3, 5).
pub const ROW_PADDING_X: f32 = 10.0;
pub const ROW_PADDING_Y: f32 = 5.0;
pub const ROW_RADIUS: f32 = 0.0;
pub const ROW_TITLE_FONT_SIZE: f32 = 11.5;
pub const ROW_META_FONT_SIZE: f32 = 10.5;
pub const ROW_LINE_HEIGHT_RATIO: f32 = 1.4;
pub const ROW_TITLE_LINE_HEIGHT: f32 = 16.0;
pub const ROW_META_LINE_HEIGHT: f32 = 15.0;
/// Row gap 0 (rule 6): rows meet, they do not float.
pub const ROW_GAP: f32 = 0.0;

/// Group labels: uppercase, 10.5 px, `text.muted` (rule 2).
pub const SECTION_LABEL_FONT_SIZE: f32 = 10.5;
pub const SECTION_LABEL_PADDING_TOP: f32 = 8.0;
pub const SECTION_LABEL_PADDING_BOTTOM: f32 = 3.0;

/// `kbd` chips (rule 7).
pub const KBD_FONT_SIZE: f32 = 10.5;
pub const KBD_RADIUS: f32 = 4.0;
pub const KBD_PADDING_X: f32 = 6.0;
pub const KBD_PADDING_Y: f32 = 1.0;
/// Chip gap 6, control gap 8, region padding 12 (rule 6).
pub const CHIP_GAP: f32 = 6.0;
pub const CONTROL_GAP: f32 = 8.0;
pub const REGION_PADDING: f32 = 12.0;

/// The one heading a surface is allowed (rule 2).
pub const HEADING_FONT_SIZE: f32 = 13.0;
/// Titles inside a surface (rule 2).
pub const TITLE_FONT_SIZE: f32 = 12.0;
/// Body / list text (rule 2).
pub const BODY_FONT_SIZE: f32 = 11.5;
/// Secondary rows and button labels (rules 2, 4).
pub const SECONDARY_FONT_SIZE: f32 = 11.0;
/// Captions (rule 2).
pub const CAPTION_FONT_SIZE: f32 = 10.5;

/// Inputs: `surfaces.sunken`, 1 px border, radius 4 (rule 3).
pub const INPUT_RADIUS: f32 = 4.0;
pub const INPUT_PADDING_X: f32 = 8.0;
pub const INPUT_PADDING_Y: f32 = 4.0;

/// The quiet toggle (a 28x16 pill with a 12 px knob) that replaces every
/// coloured switch. Off is a bare border, on is a `text.primary` fill; the knob
/// is always the canvas, so the control never introduces a colour.
pub const TOGGLE_WIDTH: f32 = 28.0;
pub const TOGGLE_HEIGHT: f32 = 16.0;
pub const TOGGLE_KNOB_SIZE: f32 = 12.0;
pub const TOGGLE_KNOB_INSET: f32 = 2.0;

/// What a list row is showing about itself. These three are the only states the
/// redesign gives a row, and every overlay row resolves its colours through
/// them rather than deciding in place.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlayRowState {
    /// Not selected and actionable. Takes `surfaces.hover` on hover.
    Idle,
    /// The keyboard's current row. `surfaces.selection` plus a white title.
    Selected,
    /// Present but not actionable. No fill, no hover, `text.disabled`.
    Disabled,
}

impl OverlayRowState {
    /// `Selected` when this is the row the keyboard is on, else `Idle`.
    pub fn selected_when(selected: bool) -> Self {
        if selected {
            Self::Selected
        } else {
            Self::Idle
        }
    }

    /// Only an idle row reacts to the pointer: a selected row already carries a
    /// fill, and a disabled one must not advertise an action it cannot run.
    pub fn takes_hover(self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// The row's own fill. `None` means the overlay surface shows through, which is
/// what an unselected row must do -- a per-row background is what made the old
/// menus read as a stack of cards rather than a list.
pub fn row_fill(state: OverlayRowState, tokens: ThemeTokens) -> Option<Color> {
    match state {
        OverlayRowState::Selected => Some(tokens.surfaces.selection),
        OverlayRowState::Idle | OverlayRowState::Disabled => None,
    }
}

/// The title line's colour (rule 5: white on the selection).
pub fn row_title_colour(state: OverlayRowState, tokens: ThemeTokens) -> Color {
    match state {
        OverlayRowState::Selected => tokens.text.emphasis,
        OverlayRowState::Idle => tokens.text.primary,
        OverlayRowState::Disabled => tokens.text.disabled,
    }
}

/// The metadata line's colour. Muted everywhere except on the selection fill,
/// where muted stops separating from the ground -- there it steps up one to
/// `text.secondary`. Deviation from rule 5, ledgered in `lane-r3-report.md`.
pub fn row_meta_colour(state: OverlayRowState, tokens: ThemeTokens) -> Color {
    match state {
        OverlayRowState::Selected => tokens.text.secondary,
        OverlayRowState::Idle => tokens.text.muted,
        OverlayRowState::Disabled => tokens.text.disabled,
    }
}

/// The overlay panel itself: `surfaces.overlay`, one hairline, radius 6, the
/// single `raised` elevation the redesign allows (rules 1, 7).
pub fn overlay_surface(id: impl Into<ElementId>, tokens: ThemeTokens) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap(px(ROW_GAP))
        .py(px(OVERLAY_PADDING_Y))
        .rounded(px(OVERLAY_RADIUS))
        .bg(tokens.surfaces.overlay.to_gpui())
        .border(px(OVERLAY_BORDER_WIDTH))
        .border_color(tokens.borders.default.to_gpui())
        .shadow_sm()
        .text_size(px(ROW_TITLE_FONT_SIZE))
        .text_color(tokens.text.primary.to_gpui())
}

/// A full-width list row. The caller adds the handlers and the lines; the
/// chrome -- padding, radius, fill, hover -- is settled here.
pub fn overlay_row(
    id: impl Into<ElementId>,
    state: OverlayRowState,
    tokens: ThemeTokens,
) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .px(px(ROW_PADDING_X))
        .py(px(ROW_PADDING_Y))
        .rounded(px(ROW_RADIUS))
        .flex()
        .flex_col()
        .when_some(row_fill(state, tokens), |row, fill| row.bg(fill.to_gpui()))
        .when(state.takes_hover(), |row| {
            row.cursor_pointer()
                .hover(|style| style.bg(tokens.surfaces.hover.to_gpui()))
        })
}

/// The title line of a row (11.5 px, rule 5).
pub fn row_title(
    text: impl Into<String>,
    state: OverlayRowState,
    tokens: ThemeTokens,
) -> AnyElement {
    div()
        .flex_1()
        .min_w(px(0.0))
        .text_size(px(ROW_TITLE_FONT_SIZE))
        .line_height(px(ROW_TITLE_LINE_HEIGHT))
        .text_color(row_title_colour(state, tokens).to_gpui())
        .child(text.into())
        .into_any_element()
}

/// The metadata line of a row (10.5 px, rule 5).
pub fn row_meta(
    text: impl Into<String>,
    state: OverlayRowState,
    tokens: ThemeTokens,
) -> AnyElement {
    div()
        .text_size(px(ROW_META_FONT_SIZE))
        .line_height(px(ROW_META_LINE_HEIGHT))
        .text_color(row_meta_colour(state, tokens).to_gpui())
        .child(text.into())
        .into_any_element()
}

/// A group label: uppercase, 10.5 px, muted (rule 2).
pub fn section_label(text: impl Into<String>, tokens: ThemeTokens) -> AnyElement {
    div()
        .w_full()
        .px(px(ROW_PADDING_X))
        .pt(px(SECTION_LABEL_PADDING_TOP))
        .pb(px(SECTION_LABEL_PADDING_BOTTOM))
        .text_size(px(SECTION_LABEL_FONT_SIZE))
        .text_color(tokens.text.muted.to_gpui())
        .child(text.into().to_uppercase())
        .into_any_element()
}

/// The one heading a surface is allowed (rule 2): 13 px semibold, primary.
pub fn heading(text: impl Into<String>, tokens: ThemeTokens) -> AnyElement {
    div()
        .text_size(px(HEADING_FONT_SIZE))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(tokens.text.primary.to_gpui())
        .child(text.into())
        .into_any_element()
}

/// A caption under a heading, or any explanatory sentence a surface carries:
/// 10.5 px, muted (rule 2).
pub fn caption(text: impl Into<String>, tokens: ThemeTokens) -> AnyElement {
    div()
        .text_size(px(CAPTION_FONT_SIZE))
        .text_color(tokens.text.muted.to_gpui())
        .child(text.into())
        .into_any_element()
}

/// The label above an input. Same vocabulary as a group label (rule 2), with no
/// padding of its own so the field's own column owns the spacing.
pub fn field_label(text: impl Into<String>, tokens: ThemeTokens) -> AnyElement {
    div()
        .text_size(px(SECTION_LABEL_FONT_SIZE))
        .text_color(tokens.text.muted.to_gpui())
        .child(text.into().to_uppercase())
        .into_any_element()
}

/// A modal dialog's surface. Same chrome as a menu, at a dialog's padding: the
/// redesign gives a dialog no card of its own beyond the overlay it already is.
pub fn dialog_surface(id: impl Into<ElementId>, tokens: ThemeTokens) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap(px(REGION_PADDING))
        .p(px(REGION_PADDING))
        .rounded(px(OVERLAY_RADIUS))
        .bg(tokens.surfaces.overlay.to_gpui())
        .border(px(OVERLAY_BORDER_WIDTH))
        .border_color(tokens.borders.default.to_gpui())
        .shadow_sm()
        .text_size(px(BODY_FONT_SIZE))
        .text_color(tokens.text.primary.to_gpui())
}

/// A keyboard chip. The mockup's `kbd`: a hairline box, radius 4, 1x6, 10.5 px,
/// muted -- never a coloured badge.
pub fn kbd_chip(text: impl Into<String>, tokens: ThemeTokens) -> AnyElement {
    div()
        .flex_none()
        .px(px(KBD_PADDING_X))
        .py(px(KBD_PADDING_Y))
        .rounded(px(KBD_RADIUS))
        .border(px(OVERLAY_BORDER_WIDTH))
        .border_color(tokens.borders.default.to_gpui())
        .text_size(px(KBD_FONT_SIZE))
        .text_color(tokens.text.muted.to_gpui())
        .child(text.into())
        .into_any_element()
}

/// A row of `kbd` chips, the footer form every overlay uses to state its keys
/// instead of spelling them out in a sentence.
pub fn kbd_hint_row(
    keys: impl IntoIterator<Item = (String, String)>,
    tokens: ThemeTokens,
) -> AnyElement {
    div()
        .w_full()
        .px(px(ROW_PADDING_X))
        .pt(px(SECTION_LABEL_PADDING_TOP))
        .pb(px(ROW_PADDING_Y))
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(CHIP_GAP))
        .children(keys.into_iter().flat_map(|(key, meaning)| {
            [
                kbd_chip(key, tokens),
                div()
                    .flex_none()
                    .text_size(px(CAPTION_FONT_SIZE))
                    .text_color(tokens.text.muted.to_gpui())
                    .child(meaning)
                    .into_any_element(),
            ]
        }))
        .into_any_element()
}

/// One quiet 11.5 px muted sentence: the whole of an empty state (rule 9), and
/// the shape a menu uses to say it has nothing to list.
pub fn quiet_sentence(text: impl Into<String>, tokens: ThemeTokens) -> AnyElement {
    div()
        .w_full()
        .px(px(ROW_PADDING_X))
        .py(px(ROW_PADDING_Y))
        .text_size(px(BODY_FONT_SIZE))
        .line_height(px(ROW_TITLE_LINE_HEIGHT))
        .text_color(tokens.text.muted.to_gpui())
        .child(text.into())
        .into_any_element()
}

/// The 28x16 toggle. Off: a bare `borders.default` outline. On: a
/// `text.primary` fill. The knob is `surfaces.canvas` in both, so the control
/// carries no colour of its own (rule 1).
pub fn quiet_toggle(on: bool, tokens: ThemeTokens) -> AnyElement {
    let knob = div()
        .flex_none()
        .size(px(TOGGLE_KNOB_SIZE))
        .rounded(px(TOGGLE_KNOB_SIZE / 2.0))
        .bg(tokens.surfaces.canvas.to_gpui());
    div()
        .flex_none()
        .w(px(TOGGLE_WIDTH))
        .h(px(TOGGLE_HEIGHT))
        .p(px(TOGGLE_KNOB_INSET))
        .rounded(px(TOGGLE_HEIGHT / 2.0))
        .flex()
        .items_center()
        .border(px(OVERLAY_BORDER_WIDTH))
        .map(|pill| {
            if on {
                pill.justify_end()
                    .bg(tokens.text.primary.to_gpui())
                    .border_color(tokens.text.primary.to_gpui())
            } else {
                pill.justify_start()
                    .border_color(tokens.borders.default.to_gpui())
            }
        })
        .child(knob)
        .into_any_element()
}

/// Rule 4's three button looks, resolved from the tokens.
///
/// Every button in the shell is a `gpui_component::Button`, which reads its
/// colours from that library's own global palette rather than from our tokens;
/// the shell bridges the two once per paint. This is the one place that states
/// what the bridge must carry, so the rule is beside the rest of the vocabulary
/// and readable by a test, instead of inline in a render function no test can
/// reach.
///
/// **One property of rule 4 is not expressible from a palette.** gpui-component
/// 0.5.1 derives a default (`Secondary`) button's border from its *background*
/// -- `border_color` returns `bg` -- unless the call site opts into
/// `.outline()`, which additionally switches on a button shadow that rule 1
/// forbids. So the default button here is unfilled with a `text.primary` label
/// and a `surfaces.hover` hover, and its 1 px `borders.default` hairline is the
/// one thing missing. Getting it needs a per-call-site change or a library that
/// separates border from fill; ledgered in `lane-r3-report.md`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ButtonPalette {
    pub default_background: Color,
    pub default_foreground: Color,
    pub default_hover: Color,
    pub default_active: Color,
    pub primary_background: Color,
    pub primary_foreground: Color,
    pub primary_hover: Color,
    pub primary_active: Color,
    pub destructive_background: Color,
    pub destructive_foreground: Color,
    pub destructive_hover: Color,
    pub destructive_active: Color,
}

/// Rule 4, read off the tokens rather than off the action palette.
///
/// Primary is deliberately `text.primary` on `canvas` and not
/// `actions.primary`: the rule says the one primary action is light-on-dark,
/// and taking it from the action tokens would let a custom theme's accent
/// reintroduce the brand tint rule 1 removed. In the built-in dark theme the
/// two already agree, which is what makes this a pin rather than a change.
pub fn button_palette(tokens: ThemeTokens) -> ButtonPalette {
    ButtonPalette {
        // No fill: the ground shows through, which is as close to rule 4's
        // "1 px border, no fill" as a palette can get here.
        default_background: tokens.surfaces.canvas,
        default_foreground: tokens.text.primary,
        default_hover: tokens.surfaces.hover,
        default_active: tokens.surfaces.selection,
        primary_background: tokens.text.primary,
        primary_foreground: tokens.surfaces.canvas,
        primary_hover: tokens.actions.primary.hover.background,
        primary_active: tokens.actions.primary.selected.background,
        // Destructive is red text on no fill (rule 4), never a red slab.
        destructive_background: tokens.surfaces.canvas,
        destructive_foreground: tokens.status.destructive,
        destructive_hover: tokens.surfaces.hover,
        destructive_active: tokens.surfaces.selection,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tokens::RuntimePreferencesSnapshot;

    fn tokens() -> ThemeTokens {
        RuntimePreferencesSnapshot::default().tokens()
    }

    #[test]
    fn an_unselected_row_lets_the_overlay_show_through() {
        assert_eq!(row_fill(OverlayRowState::Idle, tokens()), None);
        assert_eq!(row_fill(OverlayRowState::Disabled, tokens()), None);
    }

    #[test]
    fn the_selected_row_is_the_selection_surface_under_a_white_title() {
        let tokens = tokens();
        assert_eq!(
            row_fill(OverlayRowState::Selected, tokens),
            Some(tokens.surfaces.selection)
        );
        assert_eq!(
            row_title_colour(OverlayRowState::Selected, tokens),
            tokens.text.emphasis
        );
    }

    #[test]
    fn an_idle_row_titles_in_primary_and_metas_in_muted() {
        let tokens = tokens();
        assert_eq!(
            row_title_colour(OverlayRowState::Idle, tokens),
            tokens.text.primary
        );
        assert_eq!(
            row_meta_colour(OverlayRowState::Idle, tokens),
            tokens.text.muted
        );
    }

    #[test]
    fn a_disabled_row_never_advertises_an_action() {
        let tokens = tokens();
        assert!(!OverlayRowState::Disabled.takes_hover());
        assert_eq!(
            row_title_colour(OverlayRowState::Disabled, tokens),
            tokens.text.disabled
        );
    }

    #[test]
    fn only_an_idle_row_reacts_to_the_pointer() {
        assert!(OverlayRowState::Idle.takes_hover());
        assert!(!OverlayRowState::Selected.takes_hover());
    }

    #[test]
    fn selected_when_maps_the_keyboard_index_onto_the_row_state() {
        assert_eq!(
            OverlayRowState::selected_when(true),
            OverlayRowState::Selected
        );
        assert_eq!(OverlayRowState::selected_when(false), OverlayRowState::Idle);
    }

    #[test]
    fn the_default_button_is_unfilled_with_a_primary_label() {
        let tokens = tokens();
        let palette = button_palette(tokens);
        assert_eq!(palette.default_background, tokens.surfaces.canvas);
        assert_eq!(palette.default_foreground, tokens.text.primary);
        assert_eq!(palette.default_hover, tokens.surfaces.hover);
        assert_ne!(
            palette.default_background, tokens.surfaces.overlay,
            "a default button must not be a filled chip"
        );
    }

    #[test]
    fn the_primary_button_is_light_on_dark_and_carries_no_brand_tint() {
        let tokens = tokens();
        let palette = button_palette(tokens);
        assert_eq!(palette.primary_background, tokens.text.primary);
        assert_eq!(palette.primary_foreground, tokens.surfaces.canvas);
    }

    #[test]
    fn the_destructive_button_is_red_text_and_never_a_red_slab() {
        let tokens = tokens();
        let palette = button_palette(tokens);
        assert_eq!(palette.destructive_foreground, tokens.status.destructive);
        assert_eq!(palette.destructive_background, tokens.surfaces.canvas);
        assert_ne!(
            palette.destructive_background, tokens.actions.destructive.default.background,
            "the red fill is what rule 4 removes"
        );
    }

    /// Every theme, not just the built-in dark one: a custom palette must not
    /// be able to put its accent back on a button.
    #[test]
    fn every_theme_resolves_the_same_three_button_looks() {
        use crate::ui::tokens::{Density, Scale, ThemeMode};
        for mode in [ThemeMode::Dark, ThemeMode::Light, ThemeMode::HighContrast] {
            let tokens = crate::ui::tokens::theme(mode, Density::Comfortable, Scale::Scale100);
            let palette = button_palette(tokens);
            assert_eq!(palette.primary_background, tokens.text.primary, "{mode:?}");
            assert_eq!(
                palette.primary_foreground, tokens.surfaces.canvas,
                "{mode:?}"
            );
            assert_eq!(
                palette.default_background, tokens.surfaces.canvas,
                "{mode:?}"
            );
            assert_eq!(
                palette.destructive_foreground, tokens.status.destructive,
                "{mode:?}"
            );
        }
    }

    #[test]
    fn the_metrics_are_the_ones_the_mockups_specify() {
        assert_eq!(OVERLAY_RADIUS, 6.0);
        assert_eq!(OVERLAY_BORDER_WIDTH, 1.0);
        assert_eq!(OVERLAY_ANCHOR_DROP, 4.0);
        assert_eq!(ROW_PADDING_X, 10.0);
        assert_eq!(ROW_PADDING_Y, 5.0);
        assert_eq!(ROW_RADIUS, 0.0);
        assert_eq!(ROW_GAP, 0.0);
        assert_eq!(ROW_TITLE_FONT_SIZE, 11.5);
        assert_eq!(ROW_META_FONT_SIZE, 10.5);
        assert_eq!(KBD_FONT_SIZE, 10.5);
        assert_eq!(KBD_RADIUS, 4.0);
        assert_eq!(KBD_PADDING_X, 6.0);
        assert_eq!(KBD_PADDING_Y, 1.0);
        assert_eq!(TOGGLE_WIDTH, 28.0);
        assert_eq!(TOGGLE_HEIGHT, 16.0);
        assert_eq!(TOGGLE_KNOB_SIZE, 12.0);
    }

    #[test]
    fn the_row_line_heights_follow_the_shared_ratio() {
        assert!(
            (ROW_TITLE_LINE_HEIGHT - ROW_TITLE_FONT_SIZE * ROW_LINE_HEIGHT_RATIO).abs() <= 0.55
        );
        assert!((ROW_META_LINE_HEIGHT - ROW_META_FONT_SIZE * ROW_LINE_HEIGHT_RATIO).abs() <= 0.55);
    }
}
