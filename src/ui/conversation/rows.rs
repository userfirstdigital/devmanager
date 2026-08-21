//! Pure conversation derivation over the sealed journal projection.
//!
//! This module has no `gpui` imports and is unit-testable without a window.
//! The row vocabulary is CLOSED: there is no generic, diagnostic, or
//! escape-hatch variant, so an event kind nobody has mapped is structurally
//! unrenderable rather than suppressed by a denylist.

use crate::ui::renderers::{MessageRole, TimelineItemContent, TimelineItemId, TimelineItemModel};

#[cfg(test)]
use super::fixtures::{generic_item, message_item, tool_item};

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
    Success,
    Failure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEntry {
    pub identity: String,
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
        ConversationRow::TurnFold { turn_id, .. } => {
            ConversationRowKey::TurnFold(turn_id.clone())
        }
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
            identity: format!("tool:{}", view.tool_id),
            label: view.name.clone(),
            detail: view.summary.clone(),
            state: match view.state.as_str() {
                "failed" => ActivityState::Failure,
                "pending" | "running" => ActivityState::Active,
                _ => ActivityState::Success,
            },
        }),
        TimelineItemContent::Plan(view) => Some(ActivityEntry {
            identity: format!("plan:{}", view.title),
            label: "Plan".to_string(),
            detail: view.title.clone(),
            state: match view.status.as_str() {
                "failed" => ActivityState::Failure,
                "running" | "pending" => ActivityState::Active,
                _ => ActivityState::Success,
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

pub fn derive_conversation_rows(
    items: &[TimelineItemModel],
    verbosity: ConversationVerbosity,
) -> Vec<ConversationRow> {
    let mut rows: Vec<ConversationRow> = Vec::new();
    let mut pending: Vec<ActivityEntry> = Vec::new();
    let mut pending_anchor: Option<TimelineItemId> = None;

    let mut flush = |rows: &mut Vec<ConversationRow>,
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
                let text = view.markdown.plain_text();
                match view.role_kind {
                    MessageRole::Error => rows.push(ConversationRow::Error { id: item.id, text }),
                    role => rows.push(ConversationRow::Message {
                        id: item.id,
                        role,
                        text,
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
            TimelineItemContent::Tool(_) | TimelineItemContent::Plan(_) => unreachable!(
                "tool and plan items are handled by activity_entry_of above"
            ),
        }
    }

    flush(&mut rows, &mut pending, &mut pending_anchor);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_items_produce_no_conversation_row() {
        let items = vec![generic_item("session_state"), generic_item("wholly_new_kind")];
        let rows = derive_conversation_rows(&items, ConversationVerbosity::Calm);
        assert!(
            rows.is_empty(),
            "unrecognised content must be structurally unrenderable, got {rows:?}"
        );
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
            ConversationRow::Message { role: MessageRole::User, .. }
        ));
        assert!(matches!(
            &rows[1],
            ConversationRow::Message { role: MessageRole::Assistant, .. }
        ));
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
        let items = vec![tool_item("t1", "Read", "succeeded"), tool_item("t2", "Bash", "failed")];
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
        assert_eq!(derive_conversation_rows(&items, ConversationVerbosity::Calm).len(), 1);
    }

    #[test]
    fn minimal_verbosity_keeps_a_running_group() {
        let items = vec![tool_item("t1", "Bash", "running")];
        assert_eq!(derive_conversation_rows(&items, ConversationVerbosity::Minimal).len(), 1);
    }
}
