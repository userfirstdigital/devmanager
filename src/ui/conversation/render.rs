//! Target UX painting for derived conversation rows.
//!
//! Every element body here was promoted from the throwaway look prototype
//! committed at `5336cc2`, which answered whether GPUI could land the Target
//! UX treatments before production code changed. This module paints the *closed*
//! `ConversationRow` vocabulary, so there is no fallback arm and no way for
//! an unmapped provider event to reach the screen -- it never becomes a row
//! in the first place (see `rows.rs`).

use gpui::{
    div, font, px, rems, AnyElement, App, ClipboardItem, ElementId, Font, FontFeatures, FontWeight,
    InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled, Window,
};
use gpui_component::{
    text::{TextView, TextViewStyle},
    ActiveTheme,
};
use std::sync::{Arc, OnceLock};
use time::{format_description, format_description::BorrowedFormatItem, OffsetDateTime, UtcOffset};

use crate::ui::conversation::rows::{
    activity_toggle_label, ActivityEntry, ActivityKind, ActivityState, ConversationRow,
    ConversationRowKey,
};
use crate::ui::renderers::{MarkdownDocument, MessageRole};
use crate::ui::tokens::{mix_color, ThemeMode, ThemeTokens};

/// Production assistant markdown backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantMarkdownBackend {
    /// gpui-component `TextView::markdown` (GFM).
    NativeGfm,
    /// Legacy home-grown heading/paragraph/code painter (must not remain selected).
    LegacyHomeGrownBlocks,
}

/// Paint plan for one message body. Stable identity is derived from
/// the conversation row key so streaming updates and virtualization recycle the
/// same `TextView` keyed state instead of minting a fresh entity each repaint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageMarkdownPlan {
    pub backend: AssistantMarkdownBackend,
    pub selectable: bool,
    pub source: String,
    pub text_view_key: String,
}

/// Representative T3-shaped markdown used by focused render-seam tests.
pub fn representative_t3_gfm_markdown() -> &'static str {
    "\
# Release notes

Paragraph with **strong**, *emphasis*, and `inline code`.

> Calm blockquote for secondary guidance.

- Unordered item
  - Nested unordered item
1. Ordered item
2. Second ordered item
- [ ] Task item still open
- [x] Task item done

| Column | Value |
| --- | --- |
| Link | [docs](https://example.com/docs) |

```rust
fn paint() {}
```
"
}

pub fn message_text_view_key(row_key: &ConversationRowKey, user: bool) -> String {
    let role = if user { "user" } else { "assistant" };
    format!("conversation-{role}-gfm-{row_key:?}")
}

pub fn plan_message_markdown_render(
    row_key: &ConversationRowKey,
    source: &str,
    selectable: bool,
    user: bool,
) -> MessageMarkdownPlan {
    MessageMarkdownPlan {
        backend: AssistantMarkdownBackend::NativeGfm,
        selectable,
        source: source.to_string(),
        text_view_key: message_text_view_key(row_key, user),
    }
}

/// Backend actually used by [`assistant_message_element`]. Tests require this
/// to match [`AssistantMarkdownBackend::NativeGfm`] once the TextView path is live.
pub fn assistant_message_paint_backend() -> AssistantMarkdownBackend {
    AssistantMarkdownBackend::NativeGfm
}

/// Target: the user bubble caps at 80 percent of the readable measure.
const USER_BUBBLE_FRACTION: f32 = 0.80;
/// Target: 16 px radius on the user bubble.
const USER_BUBBLE_RADIUS: f32 = 16.0;
/// Target: work-entry icons occupy a 20 px slot.
const ICON_SLOT: f32 = 20.0;
/// Target: the working indicator uses three 4 px dots.
const WORKING_DOT: f32 = 4.0;
/// Target: the floating Tasks card sits slightly inside the shared 768px
/// conversation/composer measure.
const PLAN_CARD_HORIZONTAL_INSET: f32 = 22.0;
const META_REST_OPACITY: f32 = 0.0;
const META_REVEALED_OPACITY: f32 = 1.0;

/// T3 keeps chat prose at `text-sm leading-relaxed` even when navigation uses
/// Compact density. Technical answers therefore remain readable instead of
/// collapsing into the app-wide 13/18px compact metrics.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ConversationMarkdownMetrics {
    body_size: f32,
    body_line_height: f32,
    paragraph_gap_rems: f32,
    heading_sizes: [f32; 4],
}

fn conversation_markdown_metrics(
    _density: crate::ui::tokens::Density,
) -> ConversationMarkdownMetrics {
    ConversationMarkdownMetrics {
        body_size: 14.0,
        body_line_height: 23.0,
        paragraph_gap_rems: 0.65,
        heading_sizes: [17.5, 15.75, 14.0, 12.25],
    }
}

fn tabular_numeral_font() -> Font {
    // GPUI 0.2.2 can refine font features only through `Styled::font`, so
    // start from its system-UI font helper and change just the feature set.
    let mut font = font(".SystemUIFont");
    font.features = FontFeatures(Arc::new(vec![("tnum".to_string(), 1)]));
    font
}

fn message_meta_opacity(revealed: bool) -> f32 {
    if revealed {
        META_REVEALED_OPACITY
    } else {
        META_REST_OPACITY
    }
}

use crate::ui::task_cockpit::timeline::CONVERSATION_CONTENT_MAX_WIDTH;

pub fn conversation_row_element(
    row: &ConversationRow,
    tokens: ThemeTokens,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    match row {
        ConversationRow::Message {
            role: MessageRole::User,
            text,
            markdown,
            occurred_at_ms,
            ..
        } => message_row_element(
            row,
            text.clone(),
            markdown,
            *occurred_at_ms,
            true,
            false,
            tokens,
            window,
            cx,
        ),
        ConversationRow::Message {
            role: MessageRole::Reasoning,
            text,
            ..
        } => reasoning_element(text.clone(), tokens),
        ConversationRow::Message {
            text,
            markdown,
            occurred_at_ms,
            streaming,
            ..
        } => message_row_element(
            row,
            text.clone(),
            markdown,
            *occurred_at_ms,
            false,
            *streaming,
            tokens,
            window,
            cx,
        ),
        ConversationRow::Error { text, .. } => error_element(text.clone(), tokens),
        ConversationRow::Activity { entries, state, .. } => {
            activity_element(entries, *state, tokens)
        }
        ConversationRow::ActivityToggle {
            hidden,
            expanded,
            only_tools,
            ..
        } => toggle_element(
            activity_toggle_label(*hidden, *expanded, *only_tools),
            tokens,
        ),
        ConversationRow::Question {
            prompt,
            choices,
            settled_choice,
            ..
        } => question_element(prompt, choices, *settled_choice, tokens),
        ConversationRow::TurnFold {
            label, expanded, ..
        } => turn_fold_element(label, *expanded, tokens),
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
    let height = match row {
        ConversationRow::Message {
            role: MessageRole::Reasoning,
            text,
            ..
        } => 16.0 + text_lines(text) * caption_line_height,
        ConversationRow::Message {
            role: MessageRole::User,
            text,
            ..
        } => 24.0 + markdown_body_height(text, tokens) + 4.0 + caption_line_height,
        ConversationRow::Message { text, .. } => {
            4.0 + markdown_body_height(text, tokens) + 4.0 + caption_line_height
        }
        ConversationRow::Error { text, .. } => {
            32.0 + caption_line_height + text_lines(text) * line_height
        }
        ConversationRow::Activity { entries, .. } => {
            activity_row_height(entries, caption_line_height)
        }
        ConversationRow::ActivityToggle { .. } => ICON_SLOT + 4.0,
        ConversationRow::Question {
            prompt, choices, ..
        } => 28.0 + text_lines(prompt) * line_height + if choices.is_empty() { 0.0 } else { 32.0 },
        ConversationRow::TurnFold { .. } => 24.0,
        ConversationRow::Working { .. } => 20.0,
    };
    height.max(16.0).min(u32::MAX as f32) as u32
}

/// Match the native GFM block cadence closely enough for scroll anchoring and
/// follow-to-bottom bookkeeping. The previous fixed `line_count * line_height`
/// estimate flattened paragraph gaps and code/table padding, then truncated the
/// entire message at 480px. That made a long, correctly painted answer appear
/// to start above the viewport or jump while streaming.
fn markdown_body_height(text: &str, tokens: ThemeTokens) -> f32 {
    let metrics = conversation_markdown_metrics(tokens.density.density);
    let body_line = metrics.body_line_height;
    let code_line = (tokens.density.typography.caption_line_height + 2.0).max(1.0);
    let paragraph_gap = metrics.body_size * metrics.paragraph_gap_rems;
    let mut height = 0.0;
    let mut in_fence = false;
    let mut fence_lines = 0usize;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            if in_fence {
                height += 24.0 + fence_lines.max(1) as f32 * code_line + paragraph_gap;
                fence_lines = 0;
            }
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            fence_lines = fence_lines.saturating_add(1);
            continue;
        }
        if trimmed.is_empty() {
            height += paragraph_gap;
        } else if trimmed.starts_with('#') {
            height += body_line + 5.0;
        } else if trimmed.starts_with('|') {
            height += body_line + 8.0;
        } else {
            height += body_line;
        }
    }
    if in_fence {
        height += 24.0 + fence_lines.max(1) as f32 * code_line;
    }
    height.max(body_line)
}

/// Keep timeline virtualization aware of the surfaced Tasks card. The card
/// groups plan steps behind one header and one set of padding, while ordinary
/// tool activity stays on the compact 24px cadence used by
/// `work_entry_element`.
fn activity_row_height(entries: &[ActivityEntry], caption_line_height: f32) -> f32 {
    let work_count = entries
        .iter()
        .filter(|entry| entry.kind == ActivityKind::Tool)
        .count();
    let plan_count = entries
        .iter()
        .filter(|entry| entry.kind == ActivityKind::PlanStep)
        .count();
    let work_height = work_count as f32 * (ICON_SLOT + 4.0);
    if plan_count == 0 {
        return work_height.max(ICON_SLOT + 4.0);
    }

    let step_gaps = plan_count.saturating_sub(1) as f32 * 3.0;
    let plan_card_height = 6.0
        + 24.0
        + caption_line_height
        + 8.0
        + plan_count as f32 * caption_line_height
        + step_gaps;
    let section_gap = if work_count > 0 { 2.0 } else { 0.0 };
    work_height + section_gap + plan_card_height
}

/// Turn fold. The only hairline anywhere in the transcript.
fn turn_fold_element(label: &str, expanded: bool, tokens: ThemeTokens) -> AnyElement {
    let chevron = if expanded { "^" } else { "v" };
    div()
        .w_full()
        .pt(px(4.0))
        .pb(px(8.0))
        .border_b(px(1.0))
        .border_color(mix_color(tokens.surfaces.canvas, tokens.borders.subtle, 0.60).to_gpui())
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
fn user_message_element(
    row_key: &ConversationRowKey,
    text: &str,
    markdown: &MarkdownDocument,
    tokens: ThemeTokens,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let metrics = conversation_markdown_metrics(tokens.density.density);
    let view = native_markdown_view(row_key, text, markdown, true, tokens, window, cx);
    div()
        .w_full()
        .flex()
        .justify_end()
        .child(
            div()
                .min_w(px(0.0))
                .flex_shrink()
                .max_w(px(CONVERSATION_CONTENT_MAX_WIDTH * USER_BUBBLE_FRACTION))
                .p(px(12.0))
                .rounded(px(USER_BUBBLE_RADIUS))
                .bg(tokens.surfaces.raised.to_gpui())
                .text_size(px(metrics.body_size))
                .line_height(px(metrics.body_line_height))
                .text_color(tokens.text.primary.to_gpui())
                .child(view),
        )
        .into_any_element()
}

/// Message chrome remains in layout and the tab order, but is visually
/// absent until the exact row is hovered or the meta row receives keyboard
/// focus. This is the same group-hover technique Zed uses for dense row
/// actions; GPUI scopes the named group to the matching ancestor.
fn message_row_element(
    row: &ConversationRow,
    text: String,
    markdown: &MarkdownDocument,
    occurred_at_ms: Option<u64>,
    user: bool,
    streaming: bool,
    tokens: ThemeTokens,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let row_key = crate::ui::conversation::rows::conversation_row_key(row);
    let group = format!("conversation-message-{row_key:?}");
    let body = if user {
        user_message_element(&row_key, &text, markdown, tokens, window, cx)
    } else {
        assistant_message_element(&row_key, &text, markdown, tokens, window, cx)
    };

    div()
        .w_full()
        .group(group.clone())
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(body)
        .child(message_meta_element(
            group,
            text,
            occurred_at_ms,
            user,
            streaming,
            tokens,
        ))
        .into_any_element()
}

/// Cache the parsed timestamp recipe just as Zed does for dense Git-history
/// rows. Message hover must not reparse a format description while a streamed
/// conversation is repainting.
fn message_timestamp_format() -> &'static [BorrowedFormatItem<'static>] {
    static FORMAT: OnceLock<Vec<BorrowedFormatItem<'static>>> = OnceLock::new();
    FORMAT.get_or_init(|| {
        format_description::parse("[hour repr:12 padding:none]:[minute] [period case:lower]")
            .expect("valid conversation timestamp format")
    })
}

fn format_message_timestamp_at_offset(epoch_ms: u64, offset: UtcOffset) -> Option<String> {
    let timestamp =
        OffsetDateTime::from_unix_timestamp_nanos((epoch_ms as i128) * 1_000_000).ok()?;
    timestamp
        .to_offset(offset)
        .format(message_timestamp_format())
        .ok()
}

fn format_message_timestamp(epoch_ms: Option<u64>) -> Option<String> {
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    format_message_timestamp_at_offset(epoch_ms?, offset)
}

fn message_meta_element(
    group: String,
    text: String,
    occurred_at_ms: Option<u64>,
    user: bool,
    streaming: bool,
    tokens: ThemeTokens,
) -> AnyElement {
    let copy_id = (ElementId::from("copy-conversation-message"), group.clone());
    let mut meta = div()
        .w_full()
        .flex()
        .items_center()
        .justify_end()
        .gap(px(8.0))
        .pr(px(4.0))
        .opacity(message_meta_opacity(false))
        .group_hover(group, |style| style.opacity(message_meta_opacity(true)))
        .tab_index(0)
        .focus(|style| style.opacity(message_meta_opacity(true)))
        .font(tabular_numeral_font())
        .text_size(px(tokens.density.typography.caption))
        .text_color(mix_color(tokens.surfaces.canvas, tokens.text.muted, 0.55).to_gpui());

    if let Some(timestamp) = format_message_timestamp(occurred_at_ms) {
        meta = meta.child(timestamp);
    }

    if !streaming {
        meta = meta.child(
            div()
                .id(copy_id)
                .cursor_pointer()
                .hover(|style| style.text_color(tokens.text.primary.to_gpui()))
                .on_click(move |_event, _window, cx| {
                    cx.stop_propagation();
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                })
                .child("Copy"),
        );
    }

    // Reverting agent work is intentionally advertised only for user turns,
    // matching the Target UX. The mutation remains host-owned; this painter
    // cannot mint a rewind request or bypass ComposerFence authority.
    if user {
        meta = meta.child(
            div()
                .text_color(tokens.text.muted.to_gpui())
                .child("Revert"),
        );
    }

    meta.into_any_element()
}

/// Assistant turn. No surface, no border, no avatar, no role label. Body paints
/// through selectable gpui-component GFM (`TextView::markdown`) with a stable
/// keyed identity so streaming and virtualization do not remint state.
fn assistant_message_element(
    row_key: &ConversationRowKey,
    text: &str,
    markdown: &MarkdownDocument,
    tokens: ThemeTokens,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    debug_assert_eq!(
        assistant_message_paint_backend(),
        AssistantMarkdownBackend::NativeGfm
    );
    let metrics = conversation_markdown_metrics(tokens.density.density);
    let view = native_markdown_view(row_key, text, markdown, false, tokens, window, cx);

    div()
        .w_full()
        .px(px(4.0))
        .py(px(4.0))
        .text_size(px(metrics.body_size))
        .text_color(mix_color(tokens.surfaces.canvas, tokens.text.primary, 0.86).to_gpui())
        .line_height(px(metrics.body_line_height))
        .child(view)
        .into_any_element()
}

fn native_markdown_view(
    row_key: &ConversationRowKey,
    text: &str,
    markdown: &MarkdownDocument,
    user: bool,
    tokens: ThemeTokens,
    window: &mut Window,
    cx: &mut App,
) -> TextView {
    let plan = plan_message_markdown_render(row_key, text, markdown.selectable, user);
    let code_action_scope = plan.text_view_key.clone();
    let metrics = conversation_markdown_metrics(tokens.density.density);
    let mut text_style = TextViewStyle::default();
    text_style.highlight_theme = cx.theme().highlight_theme.clone();
    text_style.is_dark = !matches!(tokens.mode, ThemeMode::Light);
    TextView::markdown(
        ElementId::Name(plan.text_view_key.clone().into()),
        plan.source,
        window,
        cx,
    )
    .selectable(plan.selectable)
    .style(
        text_style
            .paragraph_gap(rems(metrics.paragraph_gap_rems))
            .heading_font_size(move |level, _base| match level {
                1 => px(metrics.heading_sizes[0]),
                2 => px(metrics.heading_sizes[1]),
                3 => px(metrics.heading_sizes[2]),
                _ => px(metrics.heading_sizes[3]),
            })
            .code_block(
                gpui::StyleRefinement::default()
                    .font(font("Cascadia Mono"))
                    .text_size(px(tokens.density.typography.caption)),
            ),
    )
    .code_block_actions(move |block, _window, _cx| {
        let copy_text = block.code().to_string();
        let lang = block
            .lang()
            .map(|name| name.to_string())
            .unwrap_or_else(|| "Code".into());
        let code_id = ElementId::Name(
            format!(
                "copy-conversation-gfm-code-{}",
                stable_code_block_hash(&format!("{code_action_scope}:{copy_text}"))
            )
            .into(),
        );
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .py(px(4.0))
            .text_size(px(11.0))
            .child(lang)
            .child(
                div()
                    .id(code_id)
                    .cursor_pointer()
                    .on_click(move |_event, _window, cx| {
                        cx.stop_propagation();
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                    })
                    .child("Copy"),
            )
    })
}

fn stable_code_block_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
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

/// Tone recolors the heading only. Work rows remain on the conversation
/// canvas at rest; their status never grows into another card surface.
fn entry_tone(state: ActivityState, tokens: ThemeTokens) -> (gpui::Rgba, &'static str) {
    match state {
        ActivityState::Success => (tokens.text.primary.to_gpui(), "-"),
        ActivityState::Active => (tokens.status.warning.to_gpui(), "!"),
        ActivityState::Pending => (tokens.text.muted.to_gpui(), "o"),
        ActivityState::Failure => (tokens.status.destructive.to_gpui(), "x"),
    }
}

/// One work / tool entry. Quiet by default; failure recolors the heading only.
fn work_entry_element(entry: &ActivityEntry, tokens: ThemeTokens) -> AnyElement {
    let (heading_color, icon) = entry_tone(entry.state, tokens);
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(2.0))
        .py(px(2.0))
        .rounded(px(6.0))
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
fn activity_element(
    entries: &[ActivityEntry],
    _state: ActivityState,
    tokens: ThemeTokens,
) -> AnyElement {
    let mut column = div().w_full().flex().flex_col().gap(px(2.0));
    let work_entries = entries
        .iter()
        .filter(|entry| entry.kind == ActivityKind::Tool);
    let plan_entries = entries
        .iter()
        .filter(|entry| entry.kind == ActivityKind::PlanStep)
        .collect::<Vec<_>>();
    for entry in work_entries {
        column = column.child(work_entry_element(entry, tokens));
    }
    if !plan_entries.is_empty() {
        column = column.child(plan_card_element(&plan_entries, tokens));
    }
    column.into_any_element()
}

fn plan_progress(entries: &[&ActivityEntry]) -> (usize, usize) {
    (
        entries
            .iter()
            .filter(|entry| entry.state == ActivityState::Success)
            .count(),
        entries.len(),
    )
}

fn plan_card_element(entries: &[&ActivityEntry], tokens: ThemeTokens) -> AnyElement {
    let (completed, total) = plan_progress(entries);
    let mut steps = div().w_full().flex().flex_col().gap(px(3.0));
    for entry in entries {
        let (symbol, color) = match entry.state {
            ActivityState::Success => ("✓", tokens.status.success),
            ActivityState::Active => ("•", tokens.actions.primary.default.background),
            ActivityState::Pending => ("○", tokens.text.muted),
            ActivityState::Failure => ("×", tokens.status.destructive),
        };
        steps = steps.child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .text_size(px(tokens.density.typography.caption))
                .line_height(px(tokens.density.typography.caption_line_height))
                .child(
                    div()
                        .flex_none()
                        .w(px(12.0))
                        .text_center()
                        .text_color(color.to_gpui())
                        .child(symbol),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_color(
                            if matches!(
                                entry.state,
                                ActivityState::Success | ActivityState::Pending
                            ) {
                                tokens.text.muted.to_gpui()
                            } else {
                                tokens.text.secondary.to_gpui()
                            },
                        )
                        .child(entry.detail.clone()),
                ),
        );
    }

    div()
        .w(px(
            CONVERSATION_CONTENT_MAX_WIDTH - 2.0 * PLAN_CARD_HORIZONTAL_INSET
        ))
        .max_w_full()
        .mx_auto()
        .mt(px(6.0))
        .px(px(14.0))
        .py(px(12.0))
        .rounded(px(tokens.density.radii.lg))
        .bg(tokens.surfaces.raised.to_gpui())
        .shadow_sm()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.0))
                .text_size(px(tokens.density.typography.caption))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tokens.text.primary.to_gpui())
                .child("☷")
                .child("Tasks")
                .child(
                    div()
                        .font(tabular_numeral_font())
                        .font_weight(FontWeight::NORMAL)
                        .text_color(tokens.text.muted.to_gpui())
                        .child(format!("{completed}/{total}")),
                ),
        )
        .child(steps)
        .into_any_element()
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
        .rounded(px(6.0))
        .font(tabular_numeral_font())
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
            .bg(mix_color(tokens.surfaces.canvas, tokens.text.muted, 0.30).to_gpui())
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
        .font(tabular_numeral_font())
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
                .text_color(mix_color(tokens.surfaces.canvas, tokens.text.muted, 0.55).to_gpui())
                .child(format!("- {step}"))
                .into_any_element()
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_assistant_document() -> MarkdownDocument {
        MarkdownDocument {
            source: String::new(),
            selectable: true,
            copyable: true,
            html_executed: false,
            prose_wraps: true,
            blocks: Vec::new(),
            pending_links: vec![],
        }
    }

    #[test]
    fn message_render_plan_preserves_exact_gfm_and_stable_role_identity() {
        let doc = sample_assistant_document();
        let row_key = ConversationRowKey::Message("stable-event".into());
        let source = representative_t3_gfm_markdown();
        let plan = plan_message_markdown_render(&row_key, source, doc.selectable, false);
        let grown = format!("{source}\ntrailing streamed chunk");
        let plan_grown = plan_message_markdown_render(&row_key, &grown, doc.selectable, false);
        let user_plan = plan_message_markdown_render(&row_key, source, doc.selectable, true);

        assert_eq!(plan.backend, AssistantMarkdownBackend::NativeGfm);
        assert!(plan.selectable);
        assert_eq!(
            plan.source, source,
            "provider Markdown must not be reconstructed"
        );
        assert_eq!(
            plan.text_view_key, plan_grown.text_view_key,
            "streaming must not mint a new TextView key"
        );
        assert_ne!(plan.text_view_key, user_plan.text_view_key);
        assert_eq!(user_plan.source, source);
    }

    #[test]
    fn assistant_message_paint_path_selects_native_gfm() {
        assert_eq!(
            assistant_message_paint_backend(),
            AssistantMarkdownBackend::NativeGfm,
            "assistant turns must paint through TextView::markdown, not the home-grown block painter"
        );
    }

    #[test]
    fn long_rich_markdown_height_is_not_truncated_at_the_legacy_480px_cap() {
        let tokens = crate::ui::tokens::theme(
            ThemeMode::Dark,
            crate::ui::tokens::Density::Comfortable,
            crate::ui::tokens::Scale::Scale100,
        );
        let text = (0..80)
            .map(|index| format!("- readable list item {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let row = ConversationRow::Message {
            id: crate::ui::renderers::TimelineItemId::Event(crate::domain::EventId::new()),
            role: MessageRole::Assistant,
            text,
            markdown: sample_assistant_document(),
            occurred_at_ms: None,
            streaming: false,
        };

        assert!(
            conversation_row_height(&row, tokens) > 480,
            "scroll/follow bookkeeping must cover the full painted answer"
        );
    }

    #[test]
    fn fenced_code_and_table_padding_are_part_of_scroll_height() {
        let tokens = crate::ui::tokens::theme(
            ThemeMode::Dark,
            crate::ui::tokens::Density::Comfortable,
            crate::ui::tokens::Scale::Scale100,
        );
        let plain = "one\ntwo\nthree\nfour";
        let rich = "```rust\none\ntwo\n```\n\n| three | four |";
        assert!(
            markdown_body_height(rich, tokens) > markdown_body_height(plain, tokens),
            "native code/table chrome adds real vertical extent"
        );
    }

    #[test]
    fn no_conversation_row_renderer_draws_a_border() {
        // KNOWN LIMITATION: this is a source-text assertion, and source-text
        // assertions decay silently. GPUI offers no way to inspect a painted
        // element's computed style from a unit test, so there is no behavioural
        // equivalent available today. Split before this test module so the
        // assertion does not count its own needle, and count `border_b`
        // separately so a changed anchor cannot silently green the guard.
        // Re-anchor this on the real element tree if a render harness lands.
        let source = include_str!("render.rs");
        let renderers = source
            .split("#[cfg(test)]")
            .next()
            .expect("renderer source precedes its tests");
        assert_eq!(
            renderers.matches(".border(px(").count(),
            0,
            "conversation rows are separated by whitespace and surface \
             lightness, never by borders"
        );
        assert_eq!(
            renderers.matches(".border_b(px(").count(),
            1,
            "exactly one hairline exists, the turn fold's -- if this is 0 the \
             anchor above has stopped matching and is guarding nothing"
        );
    }

    #[test]
    fn the_user_bubble_caps_at_eighty_percent_of_the_measure() {
        assert!((USER_BUBBLE_FRACTION - 0.80).abs() < f32::EPSILON);
    }

    #[test]
    fn the_readable_measure_matches_the_t3_reference() {
        assert_eq!(CONVERSATION_CONTENT_MAX_WIDTH, 768.0);
        assert_eq!(PLAN_CARD_HORIZONTAL_INSET, 22.0);
    }

    #[test]
    fn chat_typography_matches_t3_in_compact_and_comfortable_density() {
        let compact = conversation_markdown_metrics(crate::ui::tokens::Density::Compact);
        let comfortable = conversation_markdown_metrics(crate::ui::tokens::Density::Comfortable);

        for metrics in [compact, comfortable] {
            assert_eq!(metrics.body_size, 14.0);
            assert_eq!(metrics.body_line_height, 23.0);
            assert_eq!(metrics.paragraph_gap_rems, 0.65);
            assert_eq!(metrics.heading_sizes, [17.5, 15.75, 14.0, 12.25]);
        }
    }

    #[test]
    fn message_meta_is_invisible_at_rest_and_visible_when_revealed() {
        assert_eq!(message_meta_opacity(false), 0.0);
        assert_eq!(message_meta_opacity(true), 1.0);
    }

    #[test]
    fn message_timestamp_uses_twelve_hour_local_clock_copy() {
        assert_eq!(
            format_message_timestamp_at_offset(0, UtcOffset::UTC).as_deref(),
            Some("12:00 am")
        );
        assert_eq!(
            format_message_timestamp_at_offset(
                13 * 60 * 60 * 1_000 + 5 * 60 * 1_000,
                UtcOffset::UTC
            )
            .as_deref(),
            Some("1:05 pm")
        );
    }

    #[test]
    fn tasks_card_progress_counts_only_completed_plan_steps() {
        let completed = ActivityEntry {
            identity: "plan:one".into(),
            kind: ActivityKind::PlanStep,
            label: "Plan".into(),
            detail: "One".into(),
            state: ActivityState::Success,
        };
        let active = ActivityEntry {
            identity: "plan:two".into(),
            kind: ActivityKind::PlanStep,
            label: "Plan".into(),
            detail: "Two".into(),
            state: ActivityState::Active,
        };
        let failed = ActivityEntry {
            identity: "plan:three".into(),
            kind: ActivityKind::PlanStep,
            label: "Plan".into(),
            detail: "Three".into(),
            state: ActivityState::Failure,
        };

        assert_eq!(plan_progress(&[&completed, &active, &failed]), (1, 3));
    }

    #[test]
    fn tasks_card_height_accounts_for_header_padding_and_every_step() {
        let plan = |identity: &str| ActivityEntry {
            identity: identity.into(),
            kind: ActivityKind::PlanStep,
            label: "Plan".into(),
            detail: identity.into(),
            state: ActivityState::Active,
        };
        let tool = ActivityEntry {
            identity: "tool".into(),
            kind: ActivityKind::Tool,
            label: "Command".into(),
            detail: "cargo fmt".into(),
            state: ActivityState::Success,
        };

        let plan_only = activity_row_height(&[plan("one"), plan("two")], 16.0);
        let mixed = activity_row_height(&[tool, plan("one"), plan("two")], 16.0);

        assert_eq!(plan_only, 89.0);
        assert_eq!(mixed, 115.0);
    }
}
