//! Board facts derived from a task's semantic journal page: plan progress
//! (one segment per step) and the "doing now" text. Pure; no gpui.

use std::collections::{BTreeMap, HashMap};

use crate::domain::snapshot::{SemanticJournalFact, SemanticJournalPayload};
use crate::domain::PlanStepStatus;

use super::model::BoardProgress;

pub const DOING_NOW_MAX_CHARS: usize = 40;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoardActivity {
    pub progress: Option<BoardProgress>,
    pub doing_now: Option<String>,
}

pub fn board_activity(facts: &[SemanticJournalFact]) -> BoardActivity {
    let mut steps: BTreeMap<&str, Option<PlanStepStatus>> = BTreeMap::new();
    // Open tool calls keyed by the sequence of their call, so "doing now" is the
    // latest unresolved call rather than the lexically last call id.
    let mut open_calls: BTreeMap<u64, &str> = BTreeMap::new();
    let mut call_sequences: HashMap<&str, u64> = HashMap::new();
    let mut last_tool_sequence: Option<u64> = None;
    let mut last_reasoning_sequence: Option<u64> = None;
    for fact in facts {
        match &fact.payload {
            SemanticJournalPayload::PlanStep {
                step_id, status, ..
            } => {
                steps.insert(step_id.as_str(), PlanStepStatus::from_wire(status));
            }
            SemanticJournalPayload::ToolCall { tool_name, call_id } => {
                open_calls.insert(fact.sequence, tool_name.as_str());
                call_sequences.insert(call_id.as_str(), fact.sequence);
                last_tool_sequence = Some(fact.sequence);
            }
            SemanticJournalPayload::ToolResult { call_id, .. } => {
                if let Some(sequence) = call_sequences.remove(call_id.as_str()) {
                    open_calls.remove(&sequence);
                }
            }
            SemanticJournalPayload::ReasoningSummary { .. } => {
                last_reasoning_sequence = Some(fact.sequence);
            }
            _ => {}
        }
    }
    let progress = (!steps.is_empty()).then(|| BoardProgress {
        // An unrecognised provider status is deliberately not completion.
        completed: steps
            .values()
            .filter(|status| **status == Some(PlanStepStatus::Completed))
            .count(),
        total: steps.len(),
    });
    let doing_now = if let Some((_, tool)) = open_calls.iter().next_back() {
        Some(bound(tool, DOING_NOW_MAX_CHARS))
    } else if last_reasoning_sequence > last_tool_sequence {
        // `None` sorts below every `Some`, so this holds only when reasoning
        // actually happened and it happened after the last tool call.
        Some("Thinking".to_string())
    } else {
        None
    };
    BoardActivity {
        progress,
        doing_now,
    }
}

/// Truncate to at most `max` characters, spending the last one on an ellipsis
/// when anything was dropped. Counts characters, never bytes.
pub(crate) fn bound(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_none() {
        return head;
    }
    if max == 0 {
        return String::new();
    }
    let mut truncated: String = head.chars().take(max - 1).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::snapshot::SemanticJournalPayload as P;
    use crate::domain::{EventId, PrivacyClass};

    fn fact(sequence: u64, payload: SemanticJournalPayload) -> SemanticJournalFact {
        SemanticJournalFact {
            id: EventId::new(),
            sequence,
            occurred_at_ms: Some(sequence as i64),
            provider: "claude_code".into(),
            schema_version: 1,
            kind: "user_message".into(),
            visibility: "task".into(),
            privacy_class: PrivacyClass::LocalOnly,
            redacted: false,
            payload,
        }
    }

    #[test]
    fn no_plan_steps_means_no_progress() {
        let facts = vec![fact(1, P::UserMessage { text: "hi".into() })];
        assert_eq!(board_activity(&facts).progress, None);
    }

    #[test]
    fn progress_counts_latest_status_per_step_and_never_unknown_as_done() {
        let facts = vec![
            fact(
                1,
                P::PlanStep {
                    step_id: "1".into(),
                    title: "a".into(),
                    status: "pending".into(),
                },
            ),
            fact(
                2,
                P::PlanStep {
                    step_id: "2".into(),
                    title: "b".into(),
                    status: "pending".into(),
                },
            ),
            fact(
                3,
                P::PlanStep {
                    step_id: "1".into(),
                    title: "a".into(),
                    status: "in_progress".into(),
                },
            ),
            fact(
                4,
                P::PlanStep {
                    step_id: "1".into(),
                    title: "a".into(),
                    status: "completed".into(),
                },
            ),
            fact(
                5,
                P::PlanStep {
                    step_id: "3".into(),
                    title: "c".into(),
                    status: "weird-state".into(),
                },
            ),
        ];
        assert_eq!(
            board_activity(&facts).progress,
            Some(BoardProgress {
                completed: 1,
                total: 3
            })
        );
    }

    #[test]
    fn a_growing_list_only_grows_the_total() {
        let mut facts = vec![
            fact(
                1,
                P::PlanStep {
                    step_id: "1".into(),
                    title: "a".into(),
                    status: "completed".into(),
                },
            ),
            fact(
                2,
                P::PlanStep {
                    step_id: "2".into(),
                    title: "b".into(),
                    status: "completed".into(),
                },
            ),
        ];
        assert_eq!(
            board_activity(&facts).progress,
            Some(BoardProgress {
                completed: 2,
                total: 2
            })
        );
        facts.push(fact(
            3,
            P::PlanStep {
                step_id: "3".into(),
                title: "c".into(),
                status: "pending".into(),
            },
        ));
        assert_eq!(
            board_activity(&facts).progress,
            Some(BoardProgress {
                completed: 2,
                total: 3
            })
        );
    }

    #[test]
    fn doing_now_is_the_last_unresolved_tool_call_bounded_to_forty_chars() {
        let long = "x".repeat(100);
        let facts = vec![
            fact(
                1,
                P::ToolCall {
                    tool_name: "Read".into(),
                    call_id: "c1".into(),
                },
            ),
            fact(
                2,
                P::ToolResult {
                    call_id: "c1".into(),
                    status: "ok".into(),
                },
            ),
            fact(
                3,
                P::ToolCall {
                    tool_name: long.clone(),
                    call_id: "c2".into(),
                },
            ),
        ];
        let doing = board_activity(&facts).doing_now.expect("open tool call");
        assert_eq!(doing.chars().count(), DOING_NOW_MAX_CHARS);
        assert!(doing.ends_with('…'));
    }

    #[test]
    fn doing_now_prefers_the_latest_open_call_not_the_last_call_id() {
        // Two calls are open at once and the later one has the lexically
        // smaller id, so selecting by call id would name the stale tool.
        let facts = vec![
            fact(
                1,
                P::ToolCall {
                    tool_name: "Zed".into(),
                    call_id: "z1".into(),
                },
            ),
            fact(
                2,
                P::ToolCall {
                    tool_name: "Alpha".into(),
                    call_id: "a2".into(),
                },
            ),
        ];
        assert_eq!(board_activity(&facts).doing_now.as_deref(), Some("Alpha"));
    }

    #[test]
    fn doing_now_is_none_when_every_call_has_a_result() {
        let facts = vec![
            fact(
                1,
                P::ToolCall {
                    tool_name: "Bash".into(),
                    call_id: "c1".into(),
                },
            ),
            fact(
                2,
                P::ToolResult {
                    call_id: "c1".into(),
                    status: "ok".into(),
                },
            ),
        ];
        assert_eq!(board_activity(&facts).doing_now, None);
    }

    #[test]
    fn reasoning_after_the_last_tool_reads_as_thinking() {
        let facts = vec![
            fact(
                1,
                P::ToolCall {
                    tool_name: "Bash".into(),
                    call_id: "c1".into(),
                },
            ),
            fact(
                2,
                P::ToolResult {
                    call_id: "c1".into(),
                    status: "ok".into(),
                },
            ),
            fact(3, P::ReasoningSummary { text: "hmm".into() }),
        ];
        assert_eq!(
            board_activity(&facts).doing_now.as_deref(),
            Some("Thinking")
        );
    }
}
