//! Maps Codex rollout transcript records (`~/.codex/sessions/**/rollout-*.jsonl`)
//! to semantic events. The rollout format is Codex-internal: every mapping here
//! is tolerant, and unknown or malformed records must yield nothing rather than
//! fail the session.

use crate::remote::presentation::{
    SemanticEventDraft, SemanticEventKind, SemanticRetention, SemanticSource, SemanticStream,
    SemanticToolState, StableSessionKey,
};
use serde_json::Value;

const MAX_ROLLOUT_TEXT_BYTES: usize = 64 * 1024;
const TRUNCATION_SUFFIX: &str = "\n[truncated by DevManager]";

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn bounded_rollout_text(value: &str) -> String {
    if value.len() <= MAX_ROLLOUT_TEXT_BYTES {
        return value.to_string();
    }
    let budget = MAX_ROLLOUT_TEXT_BYTES.saturating_sub(TRUNCATION_SUFFIX.len());
    format!("{}{TRUNCATION_SUFFIX}", truncate_utf8(value, budget))
}

fn string_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

/// Joins the `output` array of a tool-call output record ([{type, text}, ...]).
fn joined_output_text(payload: &Value) -> String {
    payload
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| string_field(part, "text"))
        .collect::<Vec<_>>()
        .join("")
}

pub struct CodexRolloutReducer {
    stable_session_key: StableSessionKey,
    assistant_counter: u64,
}

impl CodexRolloutReducer {
    pub fn new(stable_session_key: StableSessionKey) -> Self {
        Self {
            stable_session_key,
            assistant_counter: 0,
        }
    }

    /// One JSONL line (no trailing newline). Malformed/unknown lines yield no drafts.
    pub fn observe_line(&mut self, line: &str, observed_at_epoch_ms: u64) -> Vec<SemanticEventDraft> {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let record_type = string_field(&record, "type").unwrap_or_default();
        let Some(payload) = record.get("payload") else {
            return Vec::new();
        };
        let payload_type = string_field(payload, "type").unwrap_or_default();

        match (record_type, payload_type) {
            ("event_msg", "agent_message") => {
                let Some(message) = string_field(payload, "message").filter(|m| !m.is_empty())
                else {
                    return Vec::new();
                };
                // agent_message records carry no id; a deterministic counter keeps
                // dedup keys stable across a re-read of the same file.
                self.assistant_counter += 1;
                let message_id = format!("rollout-assistant-{}", self.assistant_counter);
                vec![self.event(
                    observed_at_epoch_ms,
                    SemanticEventKind::AssistantMessage {
                        message_id: message_id.clone(),
                        text: bounded_rollout_text(message),
                        streaming: false,
                    },
                    SemanticRetention::Canonical,
                    format!("codex-rollout:assistant:{message_id}"),
                )]
            }
            ("event_msg", "task_started") => {
                self.turn_status(payload, observed_at_epoch_ms, "working")
            }
            ("event_msg", "task_complete") => {
                self.turn_status(payload, observed_at_epoch_ms, "idle")
            }
            ("event_msg", "turn_aborted") => {
                self.turn_status(payload, observed_at_epoch_ms, "interrupted")
            }
            ("event_msg", "token_count") => {
                let info = payload.get("info").unwrap_or(&Value::Null);
                let Some(total_tokens) = info
                    .get("total_token_usage")
                    .and_then(|usage| usage.get("total_tokens"))
                    .and_then(Value::as_u64)
                else {
                    return Vec::new();
                };
                let context_window = info.get("model_context_window").and_then(Value::as_u64);
                let detail = match context_window {
                    Some(window) => format!("{total_tokens} total tokens, {window} context window"),
                    None => format!("{total_tokens} total tokens"),
                };
                vec![self.event(
                    observed_at_epoch_ms,
                    SemanticEventKind::Status {
                        state: "usage".to_string(),
                        detail: Some(detail),
                    },
                    SemanticRetention::Verbose,
                    "codex-rollout:token-usage".to_string(),
                )]
            }
            ("response_item", "reasoning") => {
                let item_id = string_field(payload, "id").unwrap_or("unknown");
                let summary = payload
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|part| match part {
                        Value::String(text) => Some(text.as_str()),
                        other => string_field(other, "text"),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if summary.is_empty() {
                    return Vec::new();
                }
                vec![self.event(
                    observed_at_epoch_ms,
                    SemanticEventKind::Reasoning {
                        item_id: item_id.to_string(),
                        summary: bounded_rollout_text(&summary),
                    },
                    SemanticRetention::Verbose,
                    format!("codex-rollout:reasoning:{item_id}"),
                )]
            }
            ("response_item", "custom_tool_call") => {
                let Some(call_id) = string_field(payload, "call_id") else {
                    return Vec::new();
                };
                let input = string_field(payload, "input").unwrap_or_default();
                vec![self.event(
                    observed_at_epoch_ms,
                    SemanticEventKind::Command {
                        command_id: call_id.to_string(),
                        text: bounded_rollout_text(input),
                        exit_code: None,
                    },
                    SemanticRetention::Canonical,
                    format!("codex-rollout:command:{call_id}"),
                )]
            }
            ("response_item", "function_call") => {
                let Some(call_id) = string_field(payload, "call_id") else {
                    return Vec::new();
                };
                let name = string_field(payload, "name").unwrap_or("Tool");
                let arguments = string_field(payload, "arguments").unwrap_or_default();
                vec![self.event(
                    observed_at_epoch_ms,
                    SemanticEventKind::Tool {
                        tool_id: call_id.to_string(),
                        name: name.to_string(),
                        state: SemanticToolState::Running,
                        summary: bounded_rollout_text(arguments),
                    },
                    SemanticRetention::Canonical,
                    format!("codex-rollout:tool:{call_id}"),
                )]
            }
            ("response_item", "custom_tool_call_output")
            | ("response_item", "function_call_output") => {
                let Some(call_id) = string_field(payload, "call_id") else {
                    return Vec::new();
                };
                let text = joined_output_text(payload);
                if text.is_empty() {
                    return Vec::new();
                }
                vec![self.event(
                    observed_at_epoch_ms,
                    SemanticEventKind::Output {
                        stream: SemanticStream::Stdout,
                        text: bounded_rollout_text(&text),
                    },
                    SemanticRetention::Verbose,
                    format!("codex-rollout:output:{call_id}"),
                )]
            }
            // Assistant text is taken from event_msg/agent_message; user prompts
            // arrive via the UserPromptSubmit hook; developer messages are
            // internal scaffolding. Emitting response_item/message too would
            // duplicate all three.
            ("response_item", "message") => Vec::new(),
            _ => Vec::new(),
        }
    }

    fn turn_status(&self, payload: &Value, now: u64, state: &str) -> Vec<SemanticEventDraft> {
        let turn_id = string_field(payload, "turn_id").unwrap_or("unknown");
        vec![self.event(
            now,
            SemanticEventKind::Status {
                state: state.to_string(),
                detail: None,
            },
            SemanticRetention::Canonical,
            format!("codex-rollout:turn-status:{turn_id}"),
        )]
    }

    fn event(
        &self,
        occurred_at_epoch_ms: u64,
        kind: SemanticEventKind,
        retention: SemanticRetention,
        deduplication_key: String,
    ) -> SemanticEventDraft {
        SemanticEventDraft {
            stable_session_key: self.stable_session_key.clone(),
            occurred_at_epoch_ms,
            source: SemanticSource::Codex,
            kind,
            retention,
            deduplication_key: Some(deduplication_key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_reducer() -> CodexRolloutReducer {
        CodexRolloutReducer::new(StableSessionKey::from_tab("tab-1"))
    }

    #[test]
    fn agent_message_maps_to_assistant() {
        let mut reducer = test_reducer();
        let line = r#"{"timestamp":"2026-07-17T17:37:37.799Z","type":"event_msg","payload":{"type":"agent_message","message":"Working on it.","phase":"commentary"}}"#;
        let drafts = reducer.observe_line(line, 5);
        assert!(matches!(
            &drafts[0].kind,
            SemanticEventKind::AssistantMessage { text, streaming, .. }
                if text == "Working on it." && !streaming
        ));
    }

    #[test]
    fn assistant_dedup_keys_are_stable_across_rereads() {
        let line = r#"{"type":"event_msg","payload":{"type":"agent_message","message":"hello"}}"#;
        let mut first = test_reducer();
        let mut second = test_reducer();
        assert_eq!(
            first.observe_line(line, 1)[0].deduplication_key,
            second.observe_line(line, 2)[0].deduplication_key
        );
    }

    #[test]
    fn custom_tool_call_maps_to_command() {
        let mut reducer = test_reducer();
        let line = r#"{"timestamp":"t","type":"response_item","payload":{"type":"custom_tool_call","id":"ctc_1","status":"completed","call_id":"call_9","name":"exec","input":"echo hi"}}"#;
        let drafts = reducer.observe_line(line, 5);
        assert!(matches!(
            &drafts[0].kind,
            SemanticEventKind::Command { command_id, text, .. }
                if command_id == "call_9" && text == "echo hi"
        ));
    }

    #[test]
    fn reasoning_with_summary_maps_to_reasoning() {
        let mut reducer = test_reducer();
        let with_summary = r#"{"type":"response_item","payload":{"type":"reasoning","id":"rs_1","summary":["thinking about it"]}}"#;
        let drafts = reducer.observe_line(with_summary, 1);
        assert!(matches!(
            &drafts[0].kind,
            SemanticEventKind::Reasoning { summary, .. } if summary == "thinking about it"
        ));
        let empty_summary = r#"{"type":"response_item","payload":{"type":"reasoning","id":"rs_2","summary":[],"encrypted_content":"gAAA"}}"#;
        assert!(reducer.observe_line(empty_summary, 2).is_empty());
    }

    #[test]
    fn task_lifecycle_maps_to_status() {
        let mut reducer = test_reducer();
        let started = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#;
        let complete = r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1"}}"#;
        let aborted = r#"{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t1"}}"#;
        for (line, expected) in [(started, "working"), (complete, "idle"), (aborted, "interrupted")]
        {
            let drafts = reducer.observe_line(line, 1);
            assert!(matches!(
                &drafts[0].kind,
                SemanticEventKind::Status { state, .. } if state == expected
            ));
        }
    }

    #[test]
    fn token_count_maps_to_verbose_usage_status() {
        let mut reducer = test_reducer();
        let line = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":18439},"model_context_window":258400}}}"#;
        let drafts = reducer.observe_line(line, 1);
        assert_eq!(drafts[0].retention, SemanticRetention::Verbose);
        assert!(matches!(
            &drafts[0].kind,
            SemanticEventKind::Status { state, detail }
                if state == "usage"
                    && detail.as_deref() == Some("18439 total tokens, 258400 context window")
        ));
    }

    #[test]
    fn tool_call_output_maps_to_output() {
        let mut reducer = test_reducer();
        let line = r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_9","output":[{"type":"input_text","text":"Exit code: 0\n"},{"type":"input_text","text":"done"}]}}"#;
        let drafts = reducer.observe_line(line, 1);
        assert!(matches!(
            &drafts[0].kind,
            SemanticEventKind::Output { text, .. } if text == "Exit code: 0\ndone"
        ));
    }

    #[test]
    fn function_call_maps_to_tool() {
        let mut reducer = test_reducer();
        let line = r#"{"type":"response_item","payload":{"type":"function_call","id":"fc_1","name":"wait","arguments":"{\"cell_id\":\"7\"}","call_id":"call_3"}}"#;
        let drafts = reducer.observe_line(line, 1);
        assert!(matches!(
            &drafts[0].kind,
            SemanticEventKind::Tool { tool_id, name, state, .. }
                if tool_id == "call_3" && name == "wait" && *state == SemanticToolState::Running
        ));
    }

    #[test]
    fn response_messages_are_skipped() {
        let mut reducer = test_reducer();
        for role in ["user", "developer", "assistant"] {
            let line = format!(
                r#"{{"type":"response_item","payload":{{"type":"message","role":"{role}","content":[{{"type":"input_text","text":"hi"}}]}}}}"#
            );
            assert!(reducer.observe_line(&line, 1).is_empty(), "role {role}");
        }
    }

    #[test]
    fn malformed_and_unknown_lines_yield_nothing() {
        let mut reducer = test_reducer();
        assert!(reducer.observe_line("not json", 1).is_empty());
        assert!(reducer.observe_line("{\"type\":\"session_meta\"}", 1).is_empty());
        assert!(reducer
            .observe_line(r#"{"type":"world_state","payload":{"full":true}}"#, 1)
            .is_empty());
    }

    #[test]
    fn oversized_message_is_truncated() {
        let mut reducer = test_reducer();
        let big = "y".repeat(100 * 1024);
        let line = format!(
            r#"{{"type":"event_msg","payload":{{"type":"agent_message","message":"{big}"}}}}"#
        );
        let drafts = reducer.observe_line(&line, 1);
        match &drafts[0].kind {
            SemanticEventKind::AssistantMessage { text, .. } => {
                assert!(text.len() <= MAX_ROLLOUT_TEXT_BYTES);
                assert!(text.ends_with("[truncated by DevManager]"));
            }
            other => panic!("expected assistant message, got {other:?}"),
        }
    }
}
