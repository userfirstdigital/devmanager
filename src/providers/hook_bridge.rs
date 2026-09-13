//! Thin stock-hook → journal content mapping.
//!
//! These helpers only reshape **already admitted** official Claude/Codex hook
//! bodies into the provider-neutral journal envelope. They must not be called
//! with raw unauthenticated JSON: adapters gate them behind
//! `admit_and_normalize_*` / `admit_and_normalize_ingest`, which consume the
//! authenticated current-generation Claude/Codex hook registries first.
//! Transcript paths, cwd values, and rollout filenames are never copied into
//! journal identity or text. Cursor has no proven hook surface.

use crate::providers::journal::{
    JournalNormalizeError, NormalizedAdapterDelivery, MAX_CALL_ID_BYTES, MAX_JOURNAL_TEXT_BYTES,
    MAX_SOURCE_TYPE_BYTES, MAX_TOOL_NAME_BYTES,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const JOURNAL_SCHEMA_VERSION: u32 = 1;

pub(crate) fn normalize_claude_hook(
    bytes: &[u8],
    occurred_at_ms: i64,
) -> Result<NormalizedAdapterDelivery, JournalNormalizeError> {
    normalize_stock_hook(ProviderHookKind::Claude, bytes, occurred_at_ms)
}

pub(crate) fn normalize_codex_hook(
    bytes: &[u8],
    occurred_at_ms: i64,
) -> Result<NormalizedAdapterDelivery, JournalNormalizeError> {
    normalize_stock_hook(ProviderHookKind::Codex, bytes, occurred_at_ms)
}

#[derive(Clone, Copy)]
enum ProviderHookKind {
    Claude,
    Codex,
}

fn normalize_stock_hook(
    kind: ProviderHookKind,
    bytes: &[u8],
    occurred_at_ms: i64,
) -> Result<NormalizedAdapterDelivery, JournalNormalizeError> {
    if occurred_at_ms <= 0 {
        return Err(JournalNormalizeError::InvalidPayload);
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| JournalNormalizeError::InvalidPayload)?;
    let object = value
        .as_object()
        .ok_or(JournalNormalizeError::InvalidPayload)?;
    let hook_event = object
        .get("hook_event_name")
        .and_then(Value::as_str)
        .ok_or(JournalNormalizeError::InvalidPayload)?;
    reject_display(hook_event, MAX_SOURCE_TYPE_BYTES)?;

    let (source_type, payload, provider_event_id) = match hook_event {
        "SessionStart" => (
            "session_state",
            json!({ "kind": "session_state", "state": "open" }),
            stable_event_id(kind, hook_event, object.get("session_id")),
        ),
        "SessionEnd" => (
            "session_state",
            json!({ "kind": "session_state", "state": "closed" }),
            stable_event_id(kind, hook_event, object.get("session_id")),
        ),
        "UserPromptSubmit" => {
            let text = object
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or(JournalNormalizeError::InvalidPayload)?;
            reject_display(text, MAX_JOURNAL_TEXT_BYTES)?;
            (
                "user_message",
                json!({ "kind": "user_message", "text": text }),
                stable_event_id(
                    kind,
                    hook_event,
                    object.get("prompt_id").or(object.get("session_id")),
                ),
            )
        }
        "PreToolUse" => {
            let tool_name = object
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let call_id = object
                .get("tool_use_id")
                .or_else(|| object.get("call_id"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            reject_display(tool_name, MAX_TOOL_NAME_BYTES)?;
            reject_display(call_id, MAX_CALL_ID_BYTES)?;
            (
                "tool_call",
                json!({
                    "kind": "tool_call",
                    "tool_name": tool_name,
                    "call_id": call_id
                }),
                stable_event_id(kind, hook_event, Some(&Value::String(call_id.to_string()))),
            )
        }
        "PostToolUse" | "PostToolUseFailure" => {
            let call_id = object
                .get("tool_use_id")
                .or_else(|| object.get("call_id"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let status = if hook_event == "PostToolUseFailure" {
                "failed"
            } else {
                "completed"
            };
            reject_display(call_id, MAX_CALL_ID_BYTES)?;
            (
                "tool_result",
                json!({
                    "kind": "tool_result",
                    "call_id": call_id,
                    "status": status
                }),
                stable_event_id(kind, hook_event, Some(&Value::String(call_id.to_string()))),
            )
        }
        "PermissionRequest" => {
            let request_id = object
                .get("request_id")
                .or_else(|| object.get("tool_use_id"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let summary = object
                .get("message")
                .or_else(|| object.get("tool_name"))
                .and_then(Value::as_str)
                .unwrap_or("permission_required");
            reject_display(request_id, MAX_CALL_ID_BYTES)?;
            reject_display(summary, MAX_JOURNAL_TEXT_BYTES)?;
            (
                "approval_request",
                json!({
                    "kind": "approval_request",
                    "request_id": request_id,
                    "summary": summary
                }),
                stable_event_id(
                    kind,
                    hook_event,
                    Some(&Value::String(request_id.to_string())),
                ),
            )
        }
        "Stop" => (
            "turn_state",
            json!({ "kind": "turn_state", "state": "completed" }),
            stable_event_id(kind, hook_event, object.get("session_id")),
        ),
        "StopFailure" => {
            let code = object
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("stop_failure");
            let message = object
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(code);
            reject_display(code, MAX_SOURCE_TYPE_BYTES)?;
            reject_display(message, MAX_JOURNAL_TEXT_BYTES)?;
            (
                "error",
                json!({ "kind": "error", "code": code, "message": message }),
                stable_event_id(kind, hook_event, object.get("session_id")),
            )
        }
        other => {
            reject_display(other, MAX_SOURCE_TYPE_BYTES)?;
            (
                other,
                json!({ "kind": "unknown" }),
                stable_event_id(kind, other, object.get("session_id")),
            )
        }
    };

    let mut content = json!({
        "schema_version": JOURNAL_SCHEMA_VERSION,
        "source_type": source_type,
        "occurred_at_ms": occurred_at_ms,
        "payload": payload,
        "extensions": {
            "hook_event_name": hook_event
        }
    });
    if let Some(provider_event_id) = provider_event_id {
        content
            .as_object_mut()
            .ok_or(JournalNormalizeError::InvalidPayload)?
            .insert(
                "provider_event_id".to_string(),
                Value::String(provider_event_id),
            );
    }
    let encoded =
        serde_json::to_vec(&content).map_err(|_| JournalNormalizeError::InvalidPayload)?;
    NormalizedAdapterDelivery::sealed_from_content(encoded)
}

fn stable_event_id(
    kind: ProviderHookKind,
    hook_event: &str,
    seed: Option<&Value>,
) -> Option<String> {
    let seed = match seed {
        Some(Value::String(value)) if !value.is_empty() => value.as_str(),
        Some(Value::Number(value)) => {
            return Some(format!(
                "{}_{hook_event}_{value}",
                match kind {
                    ProviderHookKind::Claude => "claude",
                    ProviderHookKind::Codex => "codex",
                }
            ))
        }
        _ => return None,
    };
    let mut hasher = Sha256::new();
    hasher.update(match kind {
        ProviderHookKind::Claude => &b"claude:"[..],
        ProviderHookKind::Codex => &b"codex:"[..],
    });
    hasher.update(hook_event.as_bytes());
    hasher.update(b":");
    hasher.update(seed.as_bytes());
    let digest = hasher.finalize();
    Some(format!(
        "{}_{hook_event}_{}",
        match kind {
            ProviderHookKind::Claude => "claude",
            ProviderHookKind::Codex => "codex",
        },
        digest_prefix(&digest)
    ))
}

fn digest_prefix(digest: &[u8]) -> String {
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        out.push(core::char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(core::char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

fn reject_display(value: &str, max_bytes: usize) -> Result<(), JournalNormalizeError> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(JournalNormalizeError::InvalidPayload);
    }
    if value
        .chars()
        .any(|ch| ch.is_control() || ('\u{202A}'..='\u{202E}').contains(&ch))
    {
        return Err(JournalNormalizeError::InvalidPayload);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::claude_hooks::physically_bound_claude_hook_json;

    #[test]
    fn claude_session_start_maps_to_session_state_without_cwd_or_transcript() {
        let body = include_bytes!("../../tests/fixtures/providers/claude/session_start.json");
        physically_bound_claude_hook_json(body).expect("fixture bound");
        let delivery = normalize_claude_hook(body, 1_725_000_001_000).expect("normalize");
        let text = std::str::from_utf8(delivery.as_bytes()).expect("utf8");
        assert!(text.contains("\"kind\":\"session_state\""));
        assert!(text.contains("\"state\":\"open\""));
        assert!(text.contains("\"hook_event_name\":\"SessionStart\""));
        assert!(!text.contains("SECRET_CWD_SENTINEL"));
        assert!(!text.contains("SECRET_TRANSCRIPT_PATH_SENTINEL"));
        assert!(!text.contains("transcript_path"));
    }

    #[test]
    fn claude_user_prompt_maps_to_user_message() {
        let body = include_bytes!("../../tests/fixtures/providers/claude/user_prompt.json");
        let delivery = normalize_claude_hook(body, 1_725_000_001_000).expect("normalize");
        let text = std::str::from_utf8(delivery.as_bytes()).expect("utf8");
        assert!(text.contains("\"kind\":\"user_message\""));
        assert!(text.contains("Please inspect the reducer"));
    }

    #[test]
    fn codex_session_start_maps_without_rollout_path() {
        let body = include_bytes!("../../tests/fixtures/providers/codex/session_start.json");
        let delivery = normalize_codex_hook(body, 1_725_000_001_000).expect("normalize");
        let text = std::str::from_utf8(delivery.as_bytes()).expect("utf8");
        assert!(text.contains("\"kind\":\"session_state\""));
        assert!(!text.contains("rollout-fixture"));
        assert!(!text.contains("transcript_path"));
    }
}
