//! Target UX painting for derived conversation rows.
//!
//! Every element body here is ported from the throwaway look prototype at
//! `src/ui/conversation_preview.rs` (committed at `5336cc2`), which already
//! answered whether GPUI could land the Target UX treatments. This module is
//! the first production consumer of that prototype: it paints the *closed*
//! `ConversationRow` vocabulary, so there is no fallback arm and no way for
//! an unmapped provider event to reach the screen -- it never becomes a row
//! in the first place (see `rows.rs`).

use gpui::{div, px, AnyElement, FontWeight, IntoElement, ParentElement, Styled};

use crate::ui::conversation::rows::{
    activity_toggle_label, ActivityEntry, ActivityState, ConversationRow,
};
use crate::ui::conversation_preview::mix;
use crate::ui::renderers::MessageRole;
use crate::ui::tokens::ThemeTokens;

/// Target: the user bubble caps at 80 percent of the readable measure.
const USER_BUBBLE_FRACTION: f32 = 0.80;
/// Target: 16 px radius on the user bubble.
const USER_BUBBLE_RADIUS: f32 = 16.0;
/// Target: work-entry icons occupy a 20 px slot.
const ICON_SLOT: f32 = 20.0;
/// Target: the working indicator uses three 4 px dots.
const WORKING_DOT: f32 = 4.0;

use crate::ui::task_cockpit::timeline::CONVERSATION_CONTENT_MAX_WIDTH;

pub fn conversation_row_element(row: &ConversationRow, tokens: ThemeTokens) -> AnyElement {
    match row {
        ConversationRow::Message {
            role: MessageRole::User,
            text,
            ..
        } => user_message_element(text.clone(), tokens),
        ConversationRow::Message {
            role: MessageRole::Reasoning,
            text,
            ..
        } => reasoning_element(text.clone(), tokens),
        ConversationRow::Message { text, .. } => assistant_message_element(text.clone(), tokens),
        ConversationRow::Error { text, .. } => error_element(text.clone(), tokens),
        ConversationRow::Activity { entries, state, .. } => {
            activity_element(entries, *state, tokens)
        }
        ConversationRow::ActivityToggle {
            hidden,
            expanded,
            only_tools,
            ..
        } => toggle_element(activity_toggle_label(*hidden, *expanded, *only_tools), tokens),
        ConversationRow::Question {
            prompt,
            choices,
            settled_choice,
            ..
        } => question_element(prompt, choices, *settled_choice, tokens),
        ConversationRow::TurnFold { label, expanded, .. } => {
            turn_fold_element(label, *expanded, tokens)
        }
        ConversationRow::Working { elapsed_ms, step } => {
            working_element(*elapsed_ms, step.as_deref(), tokens)
        }
    }
}

/// Estimated paint height for one row, in the same closed vocabulary as
/// [`conversation_row_element`] and living beside it so the two cannot
/// drift. This is scroll/virtualization bookkeeping, not a layout oracle --
/// GPUI computes the real on-screen height independently when `surface()`
/// actually paints `conversation_row_element`'s output. A row that never
/// exists (a suppressed `Generic` item, for instance) never reaches this
/// function at all, because `derive_conversation_rows` never emitted it --
/// which is the whole point: no row means no height means no reserved
/// scroll space, unlike the old item-keyed estimate it replaces.
pub fn conversation_row_height(row: &ConversationRow, tokens: ThemeTokens) -> u32 {
    let line_height = tokens.density.typography.body_line_height.max(1.0);
    let caption_line_height = tokens.density.typography.caption_line_height.max(1.0);
    let text_lines = |text: &str| text.lines().count().max(1) as f32;
    match row {
        ConversationRow::Message {
            role: MessageRole::Reasoning,
            text,
            ..
        } => (16.0 + text_lines(text) * caption_line_height) as u32,
        ConversationRow::Message { text, .. } => (16.0 + text_lines(text) * line_height) as u32,
        ConversationRow::Error { text, .. } => {
            (32.0 + caption_line_height + text_lines(text) * line_height) as u32
        }
        ConversationRow::Activity { entries, .. } => {
            (entries.len().max(1) as f32 * (ICON_SLOT + 4.0)) as u32
        }
        ConversationRow::ActivityToggle { .. } => (ICON_SLOT + 4.0) as u32,
        ConversationRow::Question {
            prompt, choices, ..
        } => (28.0 + text_lines(prompt) * line_height + if choices.is_empty() { 0.0 } else { 32.0 }) as u32,
        ConversationRow::TurnFold { .. } => 24.0 as u32,
        ConversationRow::Working { .. } => 20.0 as u32,
    }
    .clamp(16, 480)
}

/// Turn fold. The only hairline anywhere in the transcript.
fn turn_fold_element(label: &str, expanded: bool, tokens: ThemeTokens) -> AnyElement {
    let chevron = if expanded { "^" } else { "v" };
    div()
        .w_full()
        .pt(px(4.0))
        .pb(px(8.0))
        .border_b(px(1.0))
        .border_color(mix(tokens.surfaces.canvas, tokens.borders.subtle, 0.60).to_gpui())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .px(px(4.0))
                .rounded(px(tokens.density.radii.sm))
                .text_size(px(tokens.density.typography.caption))
                .text_color(tokens.text.muted.to_gpui())
                .child(label.to_string())
                .child(chevron),
        )
        .into_any_element()
}

/// User turn. Right-aligned pill, the only surfaced message in the transcript.
///
/// Right alignment uses `justify_end()` on a row. Aligning with `items_end()`
/// on a column collapses its children to zero width in GPUI and they never
/// paint -- measured, not assumed, while building the prototype this ports.
fn user_message_element(text: String, tokens: ThemeTokens) -> AnyElement {
    div()
        .w_full()
        .flex()
        .justify_end()
        .child(
            div()
                // A definite width, not `w_full().max_w(..)` -- the max_w
                // form did not clamp in the prototype.
                .max_w(px(CONVERSATION_CONTENT_MAX_WIDTH * USER_BUBBLE_FRACTION))
                .p(px(12.0))
                .rounded(px(USER_BUBBLE_RADIUS))
                .bg(tokens.surfaces.raised.to_gpui())
                .text_color(tokens.text.primary.to_gpui())
                .child(text),
        )
        .into_any_element()
}

/// Assistant turn. No surface, no border, no avatar, no role label.
fn assistant_message_element(text: String, tokens: ThemeTokens) -> AnyElement {
    let mut block = div()
        .w_full()
        .px(px(4.0))
        .py(px(2.0))
        .flex()
        .flex_col()
        .gap(px(tokens.density.spacing.sm))
        .text_color(tokens.text.primary.to_gpui())
        .line_height(px(tokens.density.typography.body_line_height));
    for paragraph in text.split("\n\n") {
        if paragraph.trim().is_empty() {
            continue;
        }
        block = block.child(div().w_full().child(paragraph.to_string()));
    }
    block.into_any_element()
}

/// Reasoning turn. Quiet and visually subordinate to the assistant's answer.
fn reasoning_element(text: String, tokens: ThemeTokens) -> AnyElement {
    div()
        .w_full()
        .px(px(tokens.density.spacing.md))
        .py(px(tokens.density.spacing.sm))
        .rounded(px(tokens.density.radii.md))
        .bg(tokens.surfaces.sunken.to_gpui())
        .text_size(px(tokens.density.typography.caption))
        .text_color(tokens.text.secondary.to_gpui())
        .child(text)
        .into_any_element()
}

/// Error turn. The one message-shaped row allowed to be prominent.
fn error_element(text: String, tokens: ThemeTokens) -> AnyElement {
    div()
        .w_full()
        .px(px(tokens.density.spacing.md))
        .py(px(tokens.density.spacing.md))
        .rounded(px(tokens.density.radii.md))
        .border(px(2.0))
        .border_color(tokens.status.destructive.to_gpui())
        .bg(tokens.surfaces.raised.to_gpui())
        .flex()
        .flex_col()
        .gap(px(tokens.density.spacing.xs))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tokens.status.destructive.to_gpui())
                .child("Error"),
        )
        .child(div().text_color(tokens.text.primary.to_gpui()).child(text))
        .into_any_element()
}

/// Tone recolors the heading and, for `Active`, washes the background so a
/// running entry reads as in-progress rather than settled.
fn entry_tone(state: ActivityState, tokens: ThemeTokens) -> (gpui::Rgba, gpui::Rgba, &'static str) {
    match state {
        ActivityState::Success => (tokens.text.primary.to_gpui(), tokens.surfaces.canvas.to_gpui(), "-"),
        ActivityState::Active => (
            tokens.text.primary.to_gpui(),
            mix(tokens.surfaces.canvas, tokens.surfaces.hover, 0.20).to_gpui(),
            "!",
        ),
        ActivityState::Failure => (
            tokens.status.destructive.to_gpui(),
            tokens.surfaces.canvas.to_gpui(),
            "x",
        ),
    }
}

/// One work / tool entry. Quiet by default; failure recolors the heading only.
fn work_entry_element(entry: &ActivityEntry, tokens: ThemeTokens) -> AnyElement {
    let (heading_color, background, icon) = entry_tone(entry.state, tokens);
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(2.0))
        .py(px(2.0))
        .rounded(px(tokens.density.radii.sm))
        .bg(background)
        .child(
            div()
                .flex_none()
                .size(px(ICON_SLOT))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(tokens.density.typography.caption))
                .text_color(tokens.text.muted.to_gpui())
                .child(icon),
        )
        .child(
            div()
                .text_size(px(tokens.density.typography.caption))
                .font_weight(FontWeight::MEDIUM)
                .text_color(heading_color)
                .child(entry.label.clone()),
        )
        .children((!entry.detail.trim().is_empty()).then(|| {
            div()
                .text_size(px(tokens.density.typography.caption))
                .text_color(tokens.text.muted.to_gpui())
                .child(format!("- {}", entry.detail))
                .into_any_element()
        }))
        .into_any_element()
}

/// Activity group. Each visible entry paints as its own quiet work row.
fn activity_element(entries: &[ActivityEntry], _state: ActivityState, tokens: ThemeTokens) -> AnyElement {
    let mut column = div().w_full().flex().flex_col().gap(px(2.0));
    for entry in entries {
        column = column.child(work_entry_element(entry, tokens));
    }
    column.into_any_element()
}

/// Collapsed work group. Copy is count- and kind-aware, computed by
/// `activity_toggle_label` in the pure derivation layer.
fn toggle_element(label: String, tokens: ThemeTokens) -> AnyElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(2.0))
        .py(px(2.0))
        .rounded(px(tokens.density.radii.sm))
        .child(
            div()
                .flex_none()
                .size(px(ICON_SLOT))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(tokens.density.typography.caption))
                .text_color(tokens.text.muted.to_gpui())
                .child("v"),
        )
        .child(
            div()
                .text_size(px(tokens.density.typography.caption))
                .font_weight(FontWeight::MEDIUM)
                .text_color(tokens.text.primary.to_gpui())
                .child(label),
        )
        .into_any_element()
}

/// A composer-style pill, reused for the question row's choices.
fn choice_pill(label: &str, primary: bool, tokens: ThemeTokens) -> AnyElement {
    let (background, foreground) = if primary {
        (
            tokens.actions.primary.default.background,
            tokens.actions.primary.default.foreground,
        )
    } else {
        (tokens.surfaces.sunken, tokens.text.secondary)
    };
    div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .px(px(10.0))
        .py(px(5.0))
        .rounded(px(tokens.density.radii.pill))
        .bg(background.to_gpui())
        .text_size(px(tokens.density.typography.caption))
        .text_color(foreground.to_gpui())
        .child(label.to_string())
        .into_any_element()
}

/// Question callout. One of the few surfaces allowed to be prominent.
fn question_element(
    prompt: &str,
    choices: &[String],
    settled_choice: Option<usize>,
    tokens: ThemeTokens,
) -> AnyElement {
    let mut pills = div().flex().items_center().gap(px(8.0));
    for (index, choice) in choices.iter().enumerate() {
        let primary = settled_choice == Some(index);
        pills = pills.child(choice_pill(choice, primary, tokens));
    }
    div()
        .w_full()
        .px(px(16.0))
        .py(px(14.0))
        .rounded(px(tokens.density.radii.lg))
        .bg(tokens.surfaces.raised.to_gpui())
        .flex()
        .flex_col()
        .gap(px(tokens.density.spacing.sm))
        .child(
            div()
                .text_color(tokens.text.primary.to_gpui())
                .child(prompt.to_string()),
        )
        .child(pills)
        .into_any_element()
}

/// Working indicator. Three dots plus an elapsed label.
fn working_element(elapsed_ms: Option<u64>, step: Option<&str>, tokens: ThemeTokens) -> AnyElement {
    let dot = || {
        div()
            .flex_none()
            .size(px(WORKING_DOT))
            .rounded_full()
            .bg(mix(tokens.surfaces.canvas, tokens.text.muted, 0.30).to_gpui())
    };
    let elapsed_label = match elapsed_ms {
        None => "Working".to_string(),
        Some(ms) => {
            let total_secs = ms / 1000;
            let minutes = total_secs / 60;
            let seconds = total_secs % 60;
            if minutes > 0 {
                format!("Working for {minutes}m {seconds}s")
            } else {
                format!("Working for {seconds}s")
            }
        }
    };
    div()
        .w_full()
        .pl(px(6.0))
        .py(px(2.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(px(11.0))
        .text_color(tokens.text.secondary.to_gpui())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(3.0))
                .child(dot())
                .child(dot())
                .child(dot()),
        )
        .child(div().child(elapsed_label))
        .children(step.map(|step| {
            div()
                .text_color(mix(tokens.surfaces.canvas, tokens.text.muted, 0.55).to_gpui())
                .child(format!("- {step}"))
                .into_any_element()
        }))
        .into_any_element()
}
