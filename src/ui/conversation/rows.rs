//! Pure conversation derivation over the sealed journal projection.
//!
//! This module has no `gpui` imports and is unit-testable without a window.
//! The row vocabulary is CLOSED: there is no generic, diagnostic, or
//! escape-hatch variant, so an event kind nobody has mapped is structurally
//! unrenderable rather than suppressed by a denylist.

use crate::domain::PlanStepStatus;
use crate::ui::renderers::{
    MarkdownDocument, MessageRole, TimelineItemContent, TimelineItemId, TimelineItemModel,
};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use super::fixtures::{generic_item, message_item, plan_item, tool_item};

/// How much settled activity is derived at all. Distinct from
/// `crate::ui::tokens::Density`, which is a visual metric scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationVerbosity {
    Minimal,
    Calm,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityState {
    Active,
    Pending,
    Success,
    Failure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Tool,
    PlanStep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEntry {
    pub subagent_id: Option<String>,
    pub identity: String,
    pub kind: ActivityKind,
    pub label: String,
    pub detail: String,
    pub state: ActivityState,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConversationRowKey {
    Message(String),
    Activity(String),
    ActivityToggle(String),
    Question(String),
    Error(String),
    TurnFold(String),
    Working,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationRow {
    Message {
        id: TimelineItemId,
        role: MessageRole,
        text: String,
        markdown: MarkdownDocument,
        occurred_at_ms: Option<u64>,
        streaming: bool,
    },
    Activity {
        anchor: TimelineItemId,
        entries: Vec<ActivityEntry>,
        state: ActivityState,
        summary: String,
    },
    ActivityToggle {
        group: String,
        hidden: usize,
        expanded: bool,
        only_tools: bool,
    },
    Question {
        id: TimelineItemId,
        prompt: String,
        choices: Vec<String>,
        settled_choice: Option<usize>,
    },
    Error {
        id: TimelineItemId,
        text: String,
    },
    TurnFold {
        turn_id: String,
        label: String,
        expanded: bool,
    },
    Working {
        elapsed_ms: Option<u64>,
        step: Option<String>,
    },
}

pub fn conversation_row_key(row: &ConversationRow) -> ConversationRowKey {
    match row {
        ConversationRow::Message { id, .. } => ConversationRowKey::Message(format!("{id:?}")),
        ConversationRow::Activity { anchor, .. } => {
            ConversationRowKey::Activity(format!("{anchor:?}"))
        }
        ConversationRow::ActivityToggle { group, .. } => {
            ConversationRowKey::ActivityToggle(group.clone())
        }
        ConversationRow::Question { id, .. } => ConversationRowKey::Question(format!("{id:?}")),
        ConversationRow::Error { id, .. } => ConversationRowKey::Error(format!("{id:?}")),
        ConversationRow::TurnFold { turn_id, .. } => ConversationRowKey::TurnFold(turn_id.clone()),
        ConversationRow::Working { .. } => ConversationRowKey::Working,
    }
}

fn activity_state_of(entries: &[ActivityEntry]) -> ActivityState {
    if entries.iter().any(|e| e.state == ActivityState::Failure) {
        return ActivityState::Failure;
    }
    if entries.iter().any(|e| e.state == ActivityState::Active) {
        return ActivityState::Active;
    }
    if entries.iter().any(|e| e.state == ActivityState::Pending) {
        return ActivityState::Pending;
    }
    ActivityState::Success
}

fn activity_summary(entries: &[ActivityEntry]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for entry in entries {
        match counts.iter_mut().find(|(label, _)| label == &entry.label) {
            Some((_, count)) => *count += 1,
            None => counts.push((entry.label.clone(), 1)),
        }
    }
    let parts: Vec<String> = counts
        .into_iter()
        .map(|(label, count)| {
            if count > 1 {
                format!("{label} x{count}")
            } else {
                label
            }
        })
        .collect();
    let plural = if entries.len() == 1 { "" } else { "s" };
    format!("{} action{plural} - {}", entries.len(), parts.join(" - "))
}

/// Maps one projected item to an activity entry, or `None` when the item is
/// not activity. This match is TOTAL over `TimelineItemContent`; adding a
/// variant is a compile error here, which is the point.
fn activity_entry_of(item: &TimelineItemModel) -> Option<ActivityEntry> {
    match &item.content {
        TimelineItemContent::Tool(view) => Some(ActivityEntry {
            subagent_id: view.subagent_id.clone(),
            identity: format!("tool:{}", view.tool_id),
            kind: ActivityKind::Tool,
            label: view.name.clone(),
            detail: view.summary.clone(),
            state: match view.state.as_str() {
                "failed" => ActivityState::Failure,
                "pending" | "running" => ActivityState::Active,
                _ => ActivityState::Success,
            },
        }),
        TimelineItemContent::Plan(view) => Some(ActivityEntry {
            subagent_id: view.subagent_id.clone(),
            identity: view
                .step_id
                .as_ref()
                .map(|step_id| format!("plan:{step_id}"))
                .unwrap_or_else(|| format!("plan-event:{:?}", item.id)),
            kind: ActivityKind::PlanStep,
            label: "Plan".to_string(),
            detail: view.title.clone(),
            state: match PlanStepStatus::from_wire(&view.status) {
                Some(PlanStepStatus::Failed) => ActivityState::Failure,
                Some(PlanStepStatus::Active) => ActivityState::Active,
                Some(PlanStepStatus::Pending) => ActivityState::Pending,
                Some(PlanStepStatus::Completed) => ActivityState::Success,
                // An unrecognised provider state must never inflate Tasks x/y
                // by claiming completion. Keep it visibly unsettled.
                None => ActivityState::Pending,
            },
        }),
        TimelineItemContent::Message(_)
        | TimelineItemContent::Question(_)
        | TimelineItemContent::Approval(_)
        | TimelineItemContent::Operation(_)
        | TimelineItemContent::Artifact(_)
        | TimelineItemContent::Agent(_)
        | TimelineItemContent::Generic(_) => None,
    }
}

/// Preserves row identity across derivations so a streaming message updates
/// in place rather than re-mounting. Keys come from `conversation_row_key`,
/// which is derived from durable ids, never from content.
pub fn stable_conversation_rows(
    previous: &[ConversationRow],
    next: Vec<ConversationRow>,
) -> Vec<ConversationRow> {
    if previous.is_empty() {
        return next;
    }
    let mut prior: Vec<(ConversationRowKey, &ConversationRow)> = previous
        .iter()
        .map(|row| (conversation_row_key(row), row))
        .collect();
    next.into_iter()
        .map(|row| {
            let key = conversation_row_key(&row);
            match prior.iter().position(|(prior_key, _)| prior_key == &key) {
                Some(index) => {
                    let (_, existing) = prior.remove(index);
                    if existing == &row {
                        existing.clone()
                    } else {
                        row
                    }
                }
                None => row,
            }
        })
        .collect()
}

pub fn derive_conversation_rows(
    items: &[TimelineItemModel],
    verbosity: ConversationVerbosity,
) -> Vec<ConversationRow> {
    let mut rows: Vec<ConversationRow> = Vec::new();
    let mut pending: Vec<ActivityEntry> = Vec::new();
    let mut pending_anchor: Option<TimelineItemId> = None;
    let mut previous_turn_id: Option<String> = None;

    let flush = |rows: &mut Vec<ConversationRow>,
                 pending: &mut Vec<ActivityEntry>,
                 anchor: &mut Option<TimelineItemId>| {
        if pending.is_empty() {
            return;
        }
        let state = activity_state_of(pending);
        let drop_settled =
            verbosity == ConversationVerbosity::Minimal && state == ActivityState::Success;
        if !drop_settled {
            if let Some(anchor_id) = *anchor {
                rows.push(ConversationRow::Activity {
                    anchor: anchor_id,
                    entries: pending.clone(),
                    state,
                    summary: activity_summary(pending),
                });
            }
        }
        pending.clear();
        *anchor = None;
    };

    for item in items {
        if let Some(turn) = &item.turn_id {
            if let Some(previous) = &previous_turn_id {
                if previous != turn {
                    flush(&mut rows, &mut pending, &mut pending_anchor);
                    rows.push(ConversationRow::TurnFold {
                        turn_id: turn.clone(),
                        label: "Earlier turn".to_string(),
                        expanded: false,
                    });
                }
            }
            previous_turn_id = Some(turn.clone());
        }

        if let Some(entry) = activity_entry_of(item) {
            if pending_anchor.is_none() {
                pending_anchor = Some(item.id);
            }
            match pending.iter_mut().find(|e| e.identity == entry.identity) {
                Some(existing) => *existing = entry,
                None => pending.push(entry),
            }
            continue;
        }

        match &item.content {
            TimelineItemContent::Message(view) => {
                flush(&mut rows, &mut pending, &mut pending_anchor);
                let text = view.markdown.source.clone();
                match view.role_kind {
                    MessageRole::Error => rows.push(ConversationRow::Error { id: item.id, text }),
                    role => rows.push(ConversationRow::Message {
                        id: item.id,
                        role,
                        text,
                        markdown: view.markdown.clone(),
                        occurred_at_ms: view.occurred_at_ms,
                        streaming: view.streaming,
                    }),
                }
            }
            TimelineItemContent::Question(view) => {
                flush(&mut rows, &mut pending, &mut pending_anchor);
                rows.push(ConversationRow::Question {
                    id: item.id,
                    prompt: view.prompt.clone(),
                    choices: view.choices.clone(),
                    settled_choice: view.settled_choice,
                });
            }
            // Approvals, operations, artifacts, agents and generic extension
            // rows contribute NO conversation row. They remain addressable by
            // the inspector. This arm is what makes an unmapped kind invisible
            // by construction rather than by denylist.
            TimelineItemContent::Approval(_)
            | TimelineItemContent::Operation(_)
            | TimelineItemContent::Artifact(_)
            | TimelineItemContent::Agent(_)
            | TimelineItemContent::Generic(_) => {}
            TimelineItemContent::Tool(_) | TimelineItemContent::Plan(_) => {
                unreachable!("tool and plan items are handled by activity_entry_of above")
            }
        }
    }

    flush(&mut rows, &mut pending, &mut pending_anchor);

    let tail_is_active = matches!(
        rows.last(),
        Some(ConversationRow::Activity {
            state: ActivityState::Active,
            ..
        })
    );
    if tail_is_active {
        rows.push(ConversationRow::Working {
            elapsed_ms: None,
            step: None,
        });
    }

    rows
}

/// The folder a pasted image is staged in, and the only place the transcript
/// will treat a path as a picture of its own.
const PASTED_IMAGE_MARKER: &str = ".devmanager/pasted-images/";

/// Split the pictures out of a message.
///
/// A pasted image reaches the provider as an absolute path typed into its
/// composer, so it comes back in the echoed message as a long unreadable path.
/// The transcript shows the picture instead: the paths leave the text and are
/// returned separately.
pub fn split_pasted_images(text: &str) -> (String, Vec<String>) {
    if !text.contains(PASTED_IMAGE_MARKER) {
        return (text.to_string(), Vec::new());
    }
    let mut images = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for line in text.lines() {
        let mut remaining: Vec<&str> = Vec::new();
        for token in line.split_whitespace() {
            let candidate = token.trim_start_matches('@');
            let lower = candidate.to_ascii_lowercase();
            let is_image = candidate.contains(PASTED_IMAGE_MARKER)
                && (lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg"));
            if is_image {
                let candidate = candidate.to_string();
                if !images.contains(&candidate) {
                    images.push(candidate);
                }
            } else {
                remaining.push(token);
            }
        }
        // A line that was nothing but paths leaves no blank line behind.
        if !remaining.is_empty() {
            kept.push(remaining.join(" "));
        }
    }
    (kept.join("\n").trim().to_string(), images)
}

/// A message as this shell sent it, together with the pictures it carried.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentMessageImages {
    pub text: String,
    pub paths: Vec<String>,
}

/// Drop the markers a provider substitutes for an attached image.
///
/// Codex echoes the message back as `"[Image #1] what colour is this"` -- the
/// picture is gone and so is its path, so the transcript alone cannot say what
/// was attached. Removing the markers leaves the words that were typed, which
/// is what [`images_for_message`] matches on.
pub fn strip_provider_image_markers(text: &str) -> &str {
    let mut rest = text.trim_start();
    while let Some(open) = rest.strip_prefix("[Image #") {
        let Some(close) = open.find(']') else { break };
        if !open[..close].chars().all(|c| c.is_ascii_digit()) {
            break;
        }
        rest = open[close + 1..].trim_start();
    }
    rest
}

/// The pictures a transcript message carried, matched back to what was sent.
///
/// Matching is on the words rather than on position, so a message that arrives
/// out of order, or twice, still shows its own picture and never someone
/// else's.
pub fn images_for_message<'a>(text: &str, sent: &'a [SentMessageImages]) -> Option<&'a [String]> {
    let stripped = strip_provider_image_markers(text).trim();
    if stripped.is_empty() {
        return None;
    }
    sent.iter()
        .find(|entry| entry.text.trim() == stripped)
        .map(|entry| entry.paths.as_slice())
}

/// What the provider is doing right now, for the working row at the tail.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WorkingIndicator {
    /// When this turn started, so the row can count up. `None` keeps the bare
    /// "Working" label rather than inventing an elapsed time.
    pub since_ms: Option<i64>,
    /// The current step, e.g. a tool name or "Thinking".
    pub step: Option<String>,
}

impl WorkingIndicator {
    /// The label the row paints, and the value a caller compares to decide
    /// whether a repaint is owed. Cheap enough to call once a second.
    pub fn label(&self, now_ms: i64) -> String {
        let elapsed = self
            .since_ms
            .map(|since| format_working_elapsed((now_ms - since).max(0)))
            .unwrap_or_else(|| "Working".to_string());
        match &self.step {
            Some(step) => format!("{elapsed} · {step}"),
            None => elapsed,
        }
    }
}

/// "Working for 20s" / "Working for 3m 4s". Shared by the row and its label so
/// the two cannot drift.
pub fn format_working_elapsed(elapsed_ms: i64) -> String {
    let total = elapsed_ms.max(0) / 1000;
    let (minutes, seconds) = (total / 60, total % 60);
    if minutes > 0 {
        format!("Working for {minutes}m {seconds}s")
    } else {
        format!("Working for {seconds}s")
    }
}

/// Put a working row at the tail, or take it away.
///
/// The row used to appear only while a tool call was unresolved, so the window
/// between pressing send and the provider's first tool call -- exactly when
/// someone asks "is this stuck?" -- showed nothing at all. The caller owns the
/// question of whether a turn is open; this only paints the answer.
pub fn apply_working_indicator(
    rows: &mut Vec<ConversationRow>,
    working: Option<&WorkingIndicator>,
    now_ms: i64,
) {
    let tail_is_working = matches!(rows.last(), Some(ConversationRow::Working { .. }));
    match working {
        None => {
            if tail_is_working {
                rows.pop();
            }
        }
        Some(indicator) => {
            let row = ConversationRow::Working {
                elapsed_ms: indicator
                    .since_ms
                    .map(|since| (now_ms - since).max(0) as u64),
                step: indicator.step.clone(),
            };
            if tail_is_working {
                let last = rows.len() - 1;
                rows[last] = row;
            } else {
                rows.push(row);
            }
        }
    }
}

/// For each row, the index of the row that opened its elapsed-time window. A
/// user turn opens a boundary; a settled assistant turn closes it. This is the
/// Rust form of T3 Code's `computeMessageDurationStart`.
pub fn duration_boundaries(rows: &[ConversationRow]) -> Vec<Option<usize>> {
    let mut out = Vec::with_capacity(rows.len());
    let mut open: Option<usize> = None;
    for (index, row) in rows.iter().enumerate() {
        match row {
            ConversationRow::Message {
                role: MessageRole::User,
                ..
            } => {
                open = Some(index);
                out.push(open);
            }
            ConversationRow::Message {
                role: MessageRole::Assistant,
                streaming,
                ..
            } => {
                out.push(open.or(Some(index)));
                if !*streaming {
                    open = None;
                }
            }
            _ => out.push(open),
        }
    }
    out
}

/// Target UX: exactly one activity entry stays visible; the rest sit behind a
/// toggle. This single number is most of why the transcript reads calm.
pub const MAX_VISIBLE_ACTIVITY_ENTRIES: usize = 1;

pub fn apply_activity_collapse(
    rows: Vec<ConversationRow>,
    expanded: &[String],
) -> Vec<ConversationRow> {
    let mut out: Vec<ConversationRow> = Vec::with_capacity(rows.len());
    for row in rows {
        let ConversationRow::Activity {
            anchor,
            entries,
            state,
            summary,
        } = row
        else {
            out.push(row);
            continue;
        };
        let group = format!("{anchor:?}");
        let is_expanded = expanded.iter().any(|candidate| candidate == &group);
        if entries.len() <= MAX_VISIBLE_ACTIVITY_ENTRIES {
            out.push(ConversationRow::Activity {
                anchor,
                entries,
                state,
                summary,
            });
            continue;
        }
        let plan_count = entries
            .iter()
            .filter(|entry| entry.kind == ActivityKind::PlanStep)
            .count();
        let visible_tool_count = entries
            .iter()
            .filter(|entry| entry.kind == ActivityKind::Tool)
            .count()
            .min(MAX_VISIBLE_ACTIVITY_ENTRIES);
        let collapsed_visible = plan_count + visible_tool_count;
        if collapsed_visible >= entries.len() {
            out.push(ConversationRow::Activity {
                anchor,
                entries,
                state,
                summary,
            });
            continue;
        }
        let hidden = entries.len() - collapsed_visible;
        let only_tools = plan_count == 0;
        let shown = if is_expanded {
            entries.clone()
        } else {
            let last_visible_tool = entries
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, entry)| (entry.kind == ActivityKind::Tool).then_some(index));
            entries
                .iter()
                .enumerate()
                .filter(|(index, entry)| {
                    entry.kind == ActivityKind::PlanStep || Some(*index) == last_visible_tool
                })
                .map(|(_, entry)| entry.clone())
                .collect()
        };
        out.push(ConversationRow::Activity {
            anchor,
            entries: shown,
            state,
            summary,
        });
        out.push(ConversationRow::ActivityToggle {
            group,
            hidden,
            expanded: is_expanded,
            only_tools,
        });
    }
    out
}

/// Count- and kind-aware toggle copy, matching the Target UX exactly.
pub fn activity_toggle_label(hidden: usize, expanded: bool, only_tools: bool) -> String {
    let noun = match (only_tools, hidden) {
        (true, 1) => "previous tool call",
        (true, _) => "previous tool calls",
        (false, 1) => "previous log entry",
        (false, _) => "previous log entries",
    };
    if expanded {
        let shown = if only_tools {
            "tool calls"
        } else {
            "log entries"
        };
        format!("Show fewer {shown}")
    } else {
        format!("+{hidden} {noun}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_items_produce_no_conversation_row() {
        let items = vec![
            generic_item("session_state"),
            generic_item("wholly_new_kind"),
        ];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        assert!(
            rows.is_empty(),
            "unrecognised content must be structurally unrenderable, got {rows:?}"
        );
    }

    #[test]
    fn plan_steps_keep_active_pending_statuses_and_titles_for_the_tasks_card() {
        let rows = derive_conversation_rows(
            &[
                plan_item("step-1", "Measure the reference", "completed"),
                plan_item("step-2", "Match the shell chrome", "in_progress"),
                plan_item("step-3", "Verify the dock", "pending"),
            ],
            ConversationVerbosity::Calm,
        );

        let ConversationRow::Activity { entries, state, .. } = &rows[0] else {
            panic!("plan steps must remain one activity projection");
        };
        assert_eq!(*state, ActivityState::Active);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].detail, "Measure the reference");
        assert_eq!(entries[1].state, ActivityState::Active);
        assert_eq!(entries[2].state, ActivityState::Pending);
    }

    #[test]
    fn plan_steps_with_duplicate_titles_keep_distinct_durable_identities() {
        let rows = derive_conversation_rows(
            &[
                plan_item("step-1", "Run verification", "completed"),
                plan_item("step-2", "Run verification", "pending"),
            ],
            ConversationVerbosity::Calm,
        );
        let ConversationRow::Activity { entries, .. } = &rows[0] else {
            panic!("plan steps must remain activity entries");
        };
        assert_eq!(entries.len(), 2);
        assert_ne!(entries[0].identity, entries[1].identity);
    }

    #[test]
    fn calm_activity_collapse_keeps_every_plan_step_visible() {
        let rows = derive_conversation_rows(
            &[
                plan_item("step-1", "One", "completed"),
                plan_item("step-2", "Two", "in_progress"),
                plan_item("step-3", "Three", "pending"),
            ],
            ConversationVerbosity::Calm,
        );
        let collapsed = apply_activity_collapse(rows, &[]);
        assert!(
            collapsed
                .iter()
                .all(|row| !matches!(row, ConversationRow::ActivityToggle { .. })),
            "plan cards must not gain a dead activity fold: {collapsed:?}"
        );
        let Some(ConversationRow::Activity { entries, .. }) = collapsed
            .iter()
            .find(|row| matches!(row, ConversationRow::Activity { .. }))
        else {
            panic!("expected the plan activity row");
        };
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn an_unknown_kind_nobody_denylisted_is_still_suppressed() {
        // The regression this whole change exists to prevent.
        let items = vec![generic_item("provider_v9_telemetry_frame")];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        assert!(rows.is_empty());
    }

    #[test]
    fn user_and_assistant_messages_become_message_rows() {
        let items = vec![
            message_item(MessageRole::User, "find the flake"),
            message_item(MessageRole::Assistant, "two share a cause"),
        ];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        assert_eq!(rows.len(), 2);
        assert!(matches!(
            &rows[0],
            ConversationRow::Message {
                role: MessageRole::User,
                ..
            }
        ));
        assert!(matches!(
            &rows[1],
            ConversationRow::Message {
                role: MessageRole::Assistant,
                ..
            }
        ));
    }

    #[test]
    fn message_rows_preserve_exact_markdown_for_native_gfm_painting() {
        let source = "# Heading\n\n```rust\nfn main() {}\n```\n\n| A | B |\n|---|---|\n| 1 | 2 |";
        let rows = derive_conversation_rows(
            &[message_item(MessageRole::Assistant, source)],
            ConversationVerbosity::Calm,
        );
        let ConversationRow::Message { text, .. } = &rows[0] else {
            panic!("expected an assistant message");
        };
        assert_eq!(text, source);
        assert!(text.contains("```rust"));
        assert!(text.contains("|---|---|"));
    }

    #[test]
    fn consecutive_tools_collapse_into_one_activity_row() {
        let items = vec![
            tool_item("t1", "Read", "succeeded"),
            tool_item("t2", "Read", "succeeded"),
            tool_item("t3", "Bash", "succeeded"),
            message_item(MessageRole::Assistant, "done"),
        ];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        assert_eq!(rows.len(), 2);
        match &rows[0] {
            ConversationRow::Activity { entries, state, .. } => {
                assert_eq!(entries.len(), 3);
                assert_eq!(*state, ActivityState::Success);
            }
            other => panic!("expected an activity row, got {other:?}"),
        }
    }

    #[test]
    fn a_repeated_tool_id_replaces_rather_than_appends() {
        let items = vec![
            tool_item("t1", "Bash", "running"),
            tool_item("t1", "Bash", "succeeded"),
        ];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        match &rows[0] {
            ConversationRow::Activity { entries, state, .. } => {
                assert_eq!(entries.len(), 1, "same tool id must update in place");
                assert_eq!(*state, ActivityState::Success);
            }
            other => panic!("expected an activity row, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_tool_makes_the_group_a_failure() {
        let items = vec![
            tool_item("t1", "Read", "succeeded"),
            tool_item("t2", "Bash", "failed"),
        ];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        match &rows[0] {
            ConversationRow::Activity { state, .. } => assert_eq!(*state, ActivityState::Failure),
            other => panic!("expected an activity row, got {other:?}"),
        }
    }

    #[test]
    fn minimal_verbosity_drops_settled_successful_activity() {
        let items = vec![tool_item("t1", "Read", "succeeded")];
        assert!(derive_conversation_rows(&items, ConversationVerbosity::Minimal).is_empty());
        assert_eq!(
            derive_conversation_rows(&items, ConversationVerbosity::Calm).len(),
            1
        );
    }

    #[test]
    fn minimal_verbosity_keeps_a_running_group() {
        // This used to assert `len() == 1`. Task 4b adds a trailing `Working`
        // row whenever the tail activity group is still active (Decision 3),
        // so a running group under Minimal verbosity now derives two rows:
        // the kept Activity row this test cares about, plus Working. Assert
        // the Activity row survives rather than hardcoding a length that is
        // now a function of two independent features.
        let items = vec![tool_item("t1", "Bash", "running")];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Minimal);
        assert!(
            matches!(rows.first(), Some(ConversationRow::Activity { .. })),
            "an active group must not be dropped by minimal verbosity, got {rows:?}"
        );
    }

    #[test]
    fn row_keys_survive_a_streaming_text_change() {
        // A streaming delta is the SAME event carrying more text. `message_item`
        // mints a fresh EventId per call, so calling it twice models two
        // different messages and no key-based reconciliation could ever match
        // them -- copy the id across to model the real thing.
        let item = message_item(MessageRole::Assistant, "Two of");
        let mut grown = message_item(MessageRole::Assistant, "Two of the three");
        grown.id = item.id;

        let first = derive_conversation_rows(&[item], ConversationVerbosity::Calm);
        let second = derive_conversation_rows(&[grown], ConversationVerbosity::Calm);
        let stable = stable_conversation_rows(&first, second);
        assert_eq!(
            conversation_row_key(&first[0]),
            conversation_row_key(&stable[0]),
            "a growing message must keep its key"
        );
        match &stable[0] {
            ConversationRow::Message { text, .. } => assert_eq!(text, "Two of the three"),
            other => panic!("expected a message row, got {other:?}"),
        }
    }

    #[test]
    fn an_unchanged_row_is_returned_untouched() {
        let rows = derive_conversation_rows(
            &[message_item(MessageRole::User, "hello")],
            ConversationVerbosity::Calm,
        );
        let stable = stable_conversation_rows(&rows, rows.clone());
        assert_eq!(rows, stable);
    }

    #[test]
    fn a_group_over_the_visible_cap_gains_a_toggle() {
        let rows = derive_conversation_rows(
            &[
                tool_item("t1", "Read", "succeeded"),
                tool_item("t2", "Read", "succeeded"),
                tool_item("t3", "Bash", "succeeded"),
            ],
            ConversationVerbosity::Calm,
        );
        let collapsed = apply_activity_collapse(rows, &[]);
        assert_eq!(collapsed.len(), 2);
        match &collapsed[0] {
            ConversationRow::Activity { entries, .. } => {
                assert_eq!(entries.len(), MAX_VISIBLE_ACTIVITY_ENTRIES);
            }
            other => panic!("expected an activity row, got {other:?}"),
        }
        match &collapsed[1] {
            ConversationRow::ActivityToggle {
                hidden,
                expanded,
                only_tools,
                ..
            } => {
                assert_eq!(*hidden, 2);
                assert!(!*expanded);
                assert!(*only_tools);
            }
            other => panic!("expected a toggle row, got {other:?}"),
        }
    }

    #[test]
    fn an_expanded_group_shows_every_entry() {
        let rows = derive_conversation_rows(
            &[
                tool_item("t1", "Read", "succeeded"),
                tool_item("t2", "Read", "succeeded"),
                tool_item("t3", "Bash", "succeeded"),
            ],
            ConversationVerbosity::Calm,
        );
        let group = match &rows[0] {
            ConversationRow::Activity { anchor, .. } => format!("{anchor:?}"),
            other => panic!("expected an activity row, got {other:?}"),
        };
        let expanded = apply_activity_collapse(rows, &[group]);
        match &expanded[0] {
            ConversationRow::Activity { entries, .. } => assert_eq!(entries.len(), 3),
            other => panic!("expected an activity row, got {other:?}"),
        }
        match &expanded[1] {
            ConversationRow::ActivityToggle { expanded, .. } => assert!(*expanded),
            other => panic!("expected a toggle row, got {other:?}"),
        }
    }

    #[test]
    fn a_group_at_or_under_the_cap_gains_no_toggle() {
        let rows = derive_conversation_rows(
            &[tool_item("t1", "Read", "succeeded")],
            ConversationVerbosity::Calm,
        );
        assert_eq!(apply_activity_collapse(rows, &[]).len(), 1);
    }

    #[test]
    fn toggle_copy_is_count_and_kind_aware() {
        assert_eq!(
            activity_toggle_label(1, false, true),
            "+1 previous tool call"
        );
        assert_eq!(
            activity_toggle_label(3, false, true),
            "+3 previous tool calls"
        );
        assert_eq!(
            activity_toggle_label(1, false, false),
            "+1 previous log entry"
        );
        assert_eq!(
            activity_toggle_label(2, true, true),
            "Show fewer tool calls"
        );
        assert_eq!(
            activity_toggle_label(2, true, false),
            "Show fewer log entries"
        );
    }

    #[test]
    fn a_turn_boundary_emits_a_fold_row() {
        let mut first = message_item(MessageRole::Assistant, "older answer");
        first.turn_id = Some("turn-1".to_string());
        let mut second = message_item(MessageRole::User, "next question");
        second.turn_id = Some("turn-2".to_string());
        let rows = derive_conversation_rows(&[first, second], ConversationVerbosity::Calm);
        let folded = rows.iter().any(
            |row| matches!(row, ConversationRow::TurnFold { turn_id, .. } if turn_id == "turn-2"),
        );
        assert!(folded, "a change of turn must emit a fold, got {rows:?}");
    }

    #[test]
    fn items_without_a_turn_id_emit_no_fold() {
        let rows = derive_conversation_rows(
            &[
                message_item(MessageRole::User, "a"),
                message_item(MessageRole::Assistant, "b"),
            ],
            ConversationVerbosity::Calm,
        );
        assert!(!rows
            .iter()
            .any(|row| matches!(row, ConversationRow::TurnFold { .. })));
    }

    #[test]
    fn the_first_turn_emits_no_fold() {
        // A fold marks a boundary BETWEEN turns. On the first turn there is
        // nothing before it, so a fold would offer to collapse nothing.
        let mut only = message_item(MessageRole::User, "first question");
        only.turn_id = Some("turn-1".to_string());
        let rows = derive_conversation_rows(&[only], ConversationVerbosity::Calm);
        assert!(
            !rows
                .iter()
                .any(|row| matches!(row, ConversationRow::TurnFold { .. })),
            "the first turn must not emit a fold, got {rows:?}"
        );
    }

    #[test]
    fn the_second_turn_emits_exactly_one_fold() {
        let mut first = message_item(MessageRole::Assistant, "answer");
        first.turn_id = Some("turn-1".to_string());
        let mut second = message_item(MessageRole::User, "follow up");
        second.turn_id = Some("turn-2".to_string());
        let rows = derive_conversation_rows(&[first, second], ConversationVerbosity::Calm);
        let folds: Vec<_> = rows
            .iter()
            .filter(|row| matches!(row, ConversationRow::TurnFold { .. }))
            .collect();
        assert_eq!(folds.len(), 1, "expected exactly one fold, got {rows:?}");
    }

    #[test]
    fn a_running_tool_emits_a_working_row_at_the_tail() {
        let rows = derive_conversation_rows(
            &[tool_item("t1", "Bash", "running")],
            ConversationVerbosity::Calm,
        );
        assert!(
            matches!(rows.last(), Some(ConversationRow::Working { .. })),
            "an active group must be followed by a working row, got {rows:?}"
        );
    }

    /// The complaint this answers: a sent message that sits there with nothing
    /// on screen. No tool call has run yet, so derivation emits no working row
    /// at all -- the indicator has to come from the caller.
    #[test]
    fn a_waiting_transcript_takes_a_working_row_from_the_caller() {
        let mut rows = derive_conversation_rows(
            &[message_item(MessageRole::User, "fix the tests")],
            ConversationVerbosity::Calm,
        );
        assert!(
            !matches!(rows.last(), Some(ConversationRow::Working { .. })),
            "derivation alone still says nothing while the provider is silent"
        );

        let indicator = WorkingIndicator {
            since_ms: Some(1_000),
            step: Some("Thinking".to_string()),
        };
        apply_working_indicator(&mut rows, Some(&indicator), 21_000);
        assert!(matches!(
            rows.last(),
            Some(ConversationRow::Working {
                elapsed_ms: Some(20_000),
                step: Some(step),
            }) if step == "Thinking"
        ));

        // A later second replaces the row rather than stacking another.
        apply_working_indicator(&mut rows, Some(&indicator), 22_000);
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, ConversationRow::Working { .. }))
                .count(),
            1
        );
        assert!(matches!(
            rows.last(),
            Some(ConversationRow::Working {
                elapsed_ms: Some(21_000),
                ..
            })
        ));

        // The turn ends: the row goes, and the transcript is left alone.
        apply_working_indicator(&mut rows, None, 23_000);
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows.last(), Some(ConversationRow::Message { .. })));
    }

    /// A pasted picture reached the transcript as a wall of path, which is
    /// neither readable nor the picture.
    #[test]
    fn a_pasted_image_leaves_the_text_and_comes_back_as_a_picture() {
        let (text, images) = split_pasted_images(
            "look at this\n/home/me/proj/.devmanager/pasted-images/clipboard-image-1-2.png",
        );
        assert_eq!(text, "look at this");
        assert_eq!(
            images,
            vec!["/home/me/proj/.devmanager/pasted-images/clipboard-image-1-2.png"]
        );

        // The web path form carries an @ prefix, and two images are two rows.
        let (text, images) = split_pasted_images(
            "@.devmanager/pasted-images/a.png @.devmanager/pasted-images/b.JPG fix these",
        );
        assert_eq!(text, "fix these");
        assert_eq!(
            images,
            vec![
                ".devmanager/pasted-images/a.png",
                ".devmanager/pasted-images/b.JPG"
            ]
        );

        // The same picture twice is one picture.
        let (_, images) = split_pasted_images(
            "/w/.devmanager/pasted-images/a.png /w/.devmanager/pasted-images/a.png",
        );
        assert_eq!(images.len(), 1);

        // Ordinary text is untouched, including paths that are not images and
        // images that were never staged by the composer.
        let untouched = "see src/main.rs and https://example.com/logo.png";
        let (text, images) = split_pasted_images(untouched);
        assert_eq!(text, untouched);
        assert!(images.is_empty());
    }

    /// Codex does not echo the path back at all -- it substitutes a marker of
    /// its own -- so path-sniffing alone left every pasted picture invisible in
    /// the transcript. What this shell sent is the second source.
    #[test]
    fn a_message_the_provider_renamed_still_finds_its_picture() {
        let sent = vec![SentMessageImages {
            text: "what colour is this".into(),
            paths: vec!["/w/.devmanager/pasted-images/a.png".into()],
        }];

        assert_eq!(
            images_for_message("[Image #1] what colour is this", &sent),
            Some(&sent[0].paths[..])
        );
        // Several pictures, several markers.
        assert_eq!(
            images_for_message("[Image #1] [Image #2] what colour is this", &sent),
            Some(&sent[0].paths[..])
        );
        // Matched on the words, so the reply and an unrelated message get
        // nothing rather than someone else's picture.
        assert_eq!(images_for_message("Purple", &sent), None);
        assert_eq!(images_for_message("[Image #1]", &sent), None);
        // A marker-shaped thing that is not a marker is left alone.
        assert_eq!(
            images_for_message("[Image #x] what colour is this", &sent),
            None
        );
    }

    #[test]
    fn the_working_label_counts_up_and_names_the_step() {
        let bare = WorkingIndicator::default();
        assert_eq!(bare.label(5_000), "Working");
        let counting = WorkingIndicator {
            since_ms: Some(0),
            step: None,
        };
        assert_eq!(counting.label(20_400), "Working for 20s");
        assert_eq!(counting.label(184_000), "Working for 3m 4s");
        let stepped = WorkingIndicator {
            since_ms: Some(0),
            step: Some("Thinking".to_string()),
        };
        assert_eq!(stepped.label(20_000), "Working for 20s · Thinking");
        // A clock that jumps backwards must not print a negative age.
        assert_eq!(counting.label(-5_000), "Working for 0s");
    }

    #[test]
    fn a_settled_transcript_emits_no_working_row() {
        let rows = derive_conversation_rows(
            &[tool_item("t1", "Bash", "succeeded")],
            ConversationVerbosity::Calm,
        );
        assert!(!rows
            .iter()
            .any(|row| matches!(row, ConversationRow::Working { .. })));
    }

    #[test]
    fn a_user_message_opens_a_duration_boundary_and_a_settled_assistant_closes_it() {
        let rows = derive_conversation_rows(
            &[
                message_item(MessageRole::User, "q"),
                message_item(MessageRole::Assistant, "a"),
                message_item(MessageRole::User, "q2"),
            ],
            ConversationVerbosity::Calm,
        );
        let boundaries = duration_boundaries(&rows);
        assert_eq!(boundaries.len(), rows.len());
        assert_eq!(boundaries[0], Some(0), "a user turn opens its own boundary");
        assert_eq!(
            boundaries[1],
            Some(0),
            "the answer is measured from the question"
        );
        assert_eq!(
            boundaries[2],
            Some(2),
            "the next question opens a new boundary"
        );
    }
}
