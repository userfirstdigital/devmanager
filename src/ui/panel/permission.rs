//! The docked permission prompt, and the amber card chrome it shares with the
//! question card in the conversation stream.
//!
//! Geometry and typography are read from the approved mockup
//! `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/06-needs-you-question-1-permission.html`
//! (chosen option 1 for the question -- a card in the stream; the permission
//! prompt is the lower row of that sheet, which docks under the panel). The
//! mockup's `.qc`, `.qc .qh`, `.qc .opt`, `.qc .foot`, `.qc.perm .cmdl` and
//! `.qc .btns` rules are transcribed into the constants below so the painter
//! never approximates a size from memory, and so the question card and this
//! dock cannot drift apart: both read the same numbers.
//!
//! Every colour comes from [`ThemeTokens`]. The mockup's own hex values are
//! deliberately not repeated here -- amber is `status.attention`, the card
//! ground is `surfaces.raised` tinted toward it, and the command line sits on
//! `surfaces.sunken`.

use std::rc::Rc;

use gpui::{
    div, font, px, AnyElement, App, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window,
};

use crate::ui::tokens::{mix_color, ThemeTokens};

// ---------------------------------------------------------------------------
// `.qc` -- the card itself. Shared by the question card and the permission dock.
// ---------------------------------------------------------------------------

/// `.qc { border-radius: 7px }`.
pub const CARD_RADIUS: f32 = 7.0;
/// `.qc { padding: 8px 10px }`.
pub const CARD_PADDING_Y: f32 = 8.0;
pub const CARD_PADDING_X: f32 = 10.0;
/// `.qc { border: 1px solid }`.
pub const CARD_BORDER_WIDTH: f32 = 1.0;
/// The card ground is `surfaces.raised` mixed toward `status.attention` by this
/// much: enough that the card reads as the amber one without becoming a second
/// saturated surface (spec 5.3 keeps amber and red the only saturated colours).
pub const CARD_TINT: f32 = 0.06;
/// The card rule is the amber at just under half strength, so the card is
/// outlined rather than ringed.
pub const CARD_BORDER_ALPHA: f32 = 0.45;
/// `.qc { font-size: 12px; line-height: 1.4 }`.
pub const CARD_FONT_SIZE: f32 = 12.0;
pub const CARD_LINE_HEIGHT: f32 = 16.8;

/// `.qc .qh` -- the amber all-caps label ("QUESTION", "ALLOW?").
pub const LABEL_FONT_SIZE: f32 = 10.5;
pub const LABEL_MARGIN_BOTTOM: f32 = 4.0;

/// `.qc .qt` -- the prompt line under the label.
pub const PROMPT_MARGIN_BOTTOM: f32 = 6.0;

/// `.qc .opt` -- one choice row.
pub const CHOICE_PADDING_Y: f32 = 4.0;
pub const CHOICE_PADDING_X: f32 = 8.0;
pub const CHOICE_RADIUS: f32 = 5.0;
pub const CHOICE_GAP: f32 = 8.0;
pub const CHOICE_MARGIN_BOTTOM: f32 = 4.0;
/// `.qc .opt b { min-width: 10px }` -- the 1-based number column.
pub const CHOICE_NUMBER_WIDTH: f32 = 10.0;
/// `.qc .opt .why { font-size: 11px }` -- the trailing "recommended" note.
pub const CHOICE_NOTE_FONT_SIZE: f32 = 11.0;
/// What one choice row costs the scroll estimate: padding, rule and margin
/// around a single [`CARD_LINE_HEIGHT`] line, rounded to the spec's 26 px.
/// This is virtualization bookkeeping, not a layout oracle -- GPUI measures
/// the painted row itself.
pub const CHOICE_ROW_HEIGHT: f32 = 26.0;
/// A choice that is not the recommended one keeps the amber rule at this
/// fraction, so the recommended row is the only one that reads as a target.
pub const CHOICE_BORDER_ALPHA: f32 = 0.4;

/// `.qc .foot` -- "Type to answer in your own words" and the key hint.
pub const FOOTER_FONT_SIZE: f32 = 11.0;
pub const FOOTER_GAP: f32 = 10.0;
pub const FOOTER_MARGIN_TOP: f32 = 4.0;
/// What the label row and the footer together cost the scroll estimate.
pub const CARD_CHROME_HEIGHT: f32 = 28.0;
pub const CARD_FOOTER_HEIGHT: f32 = 22.0;

// ---------------------------------------------------------------------------
// `.qc.perm` -- the permission dock only.
// ---------------------------------------------------------------------------

/// `.qc.perm .cmdl` -- the command or edit being asked about, in the mono face.
pub const COMMAND_FONT_SIZE: f32 = 11.5;
pub const COMMAND_LINE_HEIGHT: f32 = 16.1;
pub const COMMAND_RADIUS: f32 = 4.0;
pub const COMMAND_PADDING_Y: f32 = 4.0;
pub const COMMAND_PADDING_X: f32 = 8.0;
pub const COMMAND_MARGIN_TOP: f32 = 4.0;
pub const COMMAND_MARGIN_BOTTOM: f32 = 6.0;
/// The mono face the mockup names for `.cmdl`, matching the conversation's
/// own code blocks.
pub const COMMAND_FONT: &str = "Cascadia Mono";

/// `.qc .btns span` -- the answer buttons.
pub const BUTTON_GAP: f32 = 6.0;
pub const BUTTON_RADIUS: f32 = 5.0;
pub const BUTTON_PADDING_Y: f32 = 3.0;
pub const BUTTON_PADDING_X: f32 = 9.0;
pub const BUTTON_FONT_SIZE: f32 = 11.5;
/// `.qc .btns .k` -- the right-aligned "view diff" hint.
pub const BUTTON_HINT_FONT_SIZE: f32 = 11.0;

/// The label above a permission prompt.
pub const PERMISSION_LABEL: &str = "ALLOW?";
/// The three answers, then the diff affordance.
pub const ALLOW_LABEL: &str = "Allow ⏎";
pub const ALWAYS_LABEL: &str = "Always for this task";
pub const DENY_LABEL: &str = "Deny Esc";
pub const VIEW_DIFF_LABEL: &str = "D view diff";

/// File suffixes that make a bare token a filename even without a path
/// separator. Deliberately a short curated list rather than "anything with a
/// dot": a version number, an ellipsis and a sentence all carry dots.
const FILE_SUFFIXES: [&str; 28] = [
    ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".rb", ".java", ".kt", ".c", ".h", ".cc",
    ".cpp", ".hpp", ".cs", ".swift", ".sql", ".toml", ".json", ".yaml", ".yml", ".md", ".html",
    ".css", ".sh", ".ps1", ".txt",
];

/// What the shell does when one of the four affordances is used. The painter
/// owns no state and no policy: it wires the clicks and the shell decides what
/// allowing, denying or opening a diff means for the pending approval.
pub struct PermissionHandlers {
    pub on_allow: Rc<dyn Fn(&mut Window, &mut App)>,
    pub on_always: Rc<dyn Fn(&mut Window, &mut App)>,
    pub on_deny: Rc<dyn Fn(&mut Window, &mut App)>,
    pub on_view_diff: Rc<dyn Fn(&mut Window, &mut App)>,
}

/// Whether this prompt is about a file, and therefore whether "D view diff"
/// belongs on the dock. True when any token in the summary carries a path
/// separator or ends in a known source suffix.
///
/// This is what decides whether a fourth affordance appears, so it is a rule
/// rather than a guess made at the call site: `Write src/terminal/pty.rs` has a
/// diff to show and `cargo test --lib ui::` does not.
pub fn permission_names_a_file(summary: &str) -> bool {
    summary.split_whitespace().any(|raw| {
        let token = raw.trim_matches(|character: char| {
            matches!(
                character,
                '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':' | '"' | '\'' | '`'
            )
        });
        if token.is_empty() {
            return false;
        }
        if token.contains('/') || token.contains('\\') {
            return true;
        }
        let lowered = token.to_ascii_lowercase();
        FILE_SUFFIXES
            .iter()
            .any(|suffix| lowered.len() > suffix.len() && lowered.ends_with(suffix))
    })
}

/// The amber card ground: `surfaces.raised` tinted toward the attention colour.
/// One definition, so the question card and this dock cannot paint two
/// different ambers.
pub fn card_background(tokens: ThemeTokens) -> gpui::Rgba {
    mix_color(tokens.surfaces.raised, tokens.status.attention, CARD_TINT).to_gpui()
}

/// The amber card rule.
pub fn card_border(tokens: ThemeTokens) -> gpui::Rgba {
    tokens
        .status
        .attention
        .with_alpha(CARD_BORDER_ALPHA)
        .to_gpui()
}

/// The all-caps amber label that opens every "needs you" card.
pub fn card_label(text: &str, tokens: ThemeTokens) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .mb(px(LABEL_MARGIN_BOTTOM))
        .text_size(px(LABEL_FONT_SIZE))
        .text_color(tokens.status.attention.to_gpui())
        .child(text.to_uppercase())
}

/// The permission dock: an amber card carrying the summary and the answers.
/// It replaces the compose box rather than joining the stream, so it cannot
/// scroll away while the agent is waiting.
pub fn permission_dock_element(
    summary: &str,
    tokens: ThemeTokens,
    handlers: &PermissionHandlers,
) -> AnyElement {
    let shows_diff = permission_names_a_file(summary);
    let allow = Rc::clone(&handlers.on_allow);
    let always = Rc::clone(&handlers.on_always);
    let deny = Rc::clone(&handlers.on_deny);
    let view_diff = Rc::clone(&handlers.on_view_diff);

    let button = |id: &'static str,
                  label: &'static str,
                  border: gpui::Rgba,
                  foreground: gpui::Rgba,
                  action: Rc<dyn Fn(&mut Window, &mut App)>| {
        div()
            .id(id)
            .flex_none()
            .cursor_pointer()
            .px(px(BUTTON_PADDING_X))
            .py(px(BUTTON_PADDING_Y))
            .rounded(px(BUTTON_RADIUS))
            .border(px(CARD_BORDER_WIDTH))
            .border_color(border)
            .text_size(px(BUTTON_FONT_SIZE))
            .text_color(foreground)
            .on_click(move |_event, window, cx| {
                cx.stop_propagation();
                action(window, cx);
            })
            .child(label)
    };

    let mut buttons = div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(BUTTON_GAP))
        .child(button(
            "permission-dock-allow",
            ALLOW_LABEL,
            tokens.status.attention.to_gpui(),
            tokens.status.attention.to_gpui(),
            allow,
        ))
        .child(button(
            "permission-dock-always",
            ALWAYS_LABEL,
            tokens
                .status
                .attention
                .with_alpha(CHOICE_BORDER_ALPHA)
                .to_gpui(),
            tokens.text.primary.to_gpui(),
            always,
        ))
        .child(button(
            "permission-dock-deny",
            DENY_LABEL,
            tokens
                .status
                .attention
                .with_alpha(CHOICE_BORDER_ALPHA)
                .to_gpui(),
            tokens.text.muted.to_gpui(),
            deny,
        ));
    if shows_diff {
        buttons = buttons.child(div().flex_1()).child(
            div()
                .id("permission-dock-view-diff")
                .flex_none()
                .cursor_pointer()
                .text_size(px(BUTTON_HINT_FONT_SIZE))
                .text_color(tokens.text.muted.to_gpui())
                .on_click(move |_event, window, cx| {
                    cx.stop_propagation();
                    view_diff(window, cx);
                })
                .child(VIEW_DIFF_LABEL),
        );
    }

    div()
        .w_full()
        .flex()
        .flex_col()
        .px(px(CARD_PADDING_X))
        .py(px(CARD_PADDING_Y))
        .rounded(px(CARD_RADIUS))
        .border(px(CARD_BORDER_WIDTH))
        .border_color(card_border(tokens))
        .bg(card_background(tokens))
        .text_size(px(CARD_FONT_SIZE))
        .line_height(px(CARD_LINE_HEIGHT))
        .text_color(tokens.text.primary.to_gpui())
        .child(card_label(PERMISSION_LABEL, tokens))
        .child(
            div()
                .w_full()
                .mt(px(COMMAND_MARGIN_TOP))
                .mb(px(COMMAND_MARGIN_BOTTOM))
                .px(px(COMMAND_PADDING_X))
                .py(px(COMMAND_PADDING_Y))
                .rounded(px(COMMAND_RADIUS))
                .bg(tokens.surfaces.sunken.to_gpui())
                .font(font(COMMAND_FONT))
                .text_size(px(COMMAND_FONT_SIZE))
                .line_height(px(COMMAND_LINE_HEIGHT))
                .overflow_hidden()
                .child(summary.to_string()),
        )
        .child(buttons)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_naming_a_file_offers_the_diff() {
        assert!(permission_names_a_file(
            "Write src/terminal/pty.rs (+41 -6)"
        ));
        assert!(permission_names_a_file("Edit C:\\Code\\x\\main.rs"));
        assert!(!permission_names_a_file("cargo test --lib ui::"));
    }

    #[test]
    fn a_bare_filename_still_names_a_file_and_a_version_does_not() {
        assert!(permission_names_a_file("Edit main.rs"));
        assert!(permission_names_a_file("Write Cargo.toml"));
        assert!(!permission_names_a_file("cargo build --release"));
        assert!(!permission_names_a_file("npm install react@18.3.1"));
    }

    #[test]
    fn the_card_ground_is_raised_tinted_toward_amber_not_the_amber_itself() {
        let tokens = crate::ui::tokens::dark(
            crate::ui::tokens::Density::Comfortable,
            crate::ui::tokens::Scale::Scale100,
        );
        let tinted = mix_color(tokens.surfaces.raised, tokens.status.attention, CARD_TINT);
        assert_ne!(tinted, tokens.surfaces.raised);
        assert_ne!(tinted, tokens.status.attention);
        // A 6% tint stays far closer to the surface than to the amber.
        let toward_amber = tinted.red().abs_diff(tokens.status.attention.red());
        let toward_surface = tinted.red().abs_diff(tokens.surfaces.raised.red());
        assert!(toward_surface < toward_amber);
    }
}
