use crate::remote::presentation::{
    SemanticAdapterHealth, SemanticEventDraft, SemanticEventKind, SemanticRetention,
    SemanticSource, SemanticToolState, StableSessionKey,
};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MAX_CLAUDE_HOOK_BODY_BYTES: usize = 256 * 1024;
const MAX_PROVIDER_TEXT_BYTES: usize = 48 * 1024;
const MAX_CLAUDE_SETTINGS_BYTES: usize = 1024 * 1024;
const CLAUDE_NONCE_BYTES: usize = 32;
const CLAUDE_SETTINGS_TOKEN_BYTES: usize = 16;
const CLAUDE_ACTIVATION_GRACE: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeReducerLimits {
    pub max_tool_records: usize,
    pub max_message_records: usize,
    pub max_message_batches_per_record: usize,
    pub max_message_accumulated_bytes: usize,
}

impl Default for ClaudeReducerLimits {
    fn default() -> Self {
        Self {
            max_tool_records: 512,
            max_message_records: 128,
            max_message_batches_per_record: 512,
            max_message_accumulated_bytes: MAX_PROVIDER_TEXT_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeToolSnapshot {
    pub tool_use_id: String,
    pub name: String,
    pub state: SemanticToolState,
}

#[derive(Debug, Clone)]
struct ToolRecord {
    snapshot: ClaudeToolSnapshot,
    touched: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ToolKey {
    provider_session_id: String,
    tool_use_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MessageKey {
    provider_session_id: String,
    turn_id: String,
    message_id: String,
}

#[derive(Debug, Clone)]
struct MessageBatch {
    delta: String,
    final_chunk: bool,
}

#[derive(Debug, Clone)]
struct MessageRecord {
    batches: BTreeMap<u64, MessageBatch>,
    next_index: u64,
    text: String,
    finalized: bool,
    truncated: bool,
    accumulated_bytes: usize,
    touched: u64,
}

#[derive(Debug, Clone)]
pub struct ClaudeReduceOutcome {
    pub drafts: Vec<SemanticEventDraft>,
    pub degraded: bool,
}

impl ClaudeReduceOutcome {
    fn ignored() -> Self {
        Self {
            drafts: Vec::new(),
            degraded: false,
        }
    }

    fn malformed() -> Self {
        Self {
            drafts: Vec::new(),
            degraded: true,
        }
    }
}

pub struct ClaudeReducer {
    stable_session_key: StableSessionKey,
    fallback_provider_session_id: String,
    limits: ClaudeReducerLimits,
    tools: HashMap<ToolKey, ToolRecord>,
    tool_clock: u64,
    messages: HashMap<MessageKey, MessageRecord>,
    message_clock: u64,
    event_clock: u64,
}

impl ClaudeReducer {
    pub fn new(stable_session_key: StableSessionKey, limits: ClaudeReducerLimits) -> Self {
        Self::with_fallback_provider_session_id(
            stable_session_key,
            limits,
            "standalone".to_string(),
        )
    }

    fn with_fallback_provider_session_id(
        stable_session_key: StableSessionKey,
        limits: ClaudeReducerLimits,
        fallback_provider_session_id: String,
    ) -> Self {
        Self {
            stable_session_key,
            fallback_provider_session_id,
            limits,
            tools: HashMap::new(),
            tool_clock: 0,
            messages: HashMap::new(),
            message_clock: 0,
            event_clock: 0,
        }
    }

    pub fn tool(&self, tool_use_id: &str) -> Option<ClaudeToolSnapshot> {
        self.tools
            .iter()
            .filter(|(key, _)| key.tool_use_id == tool_use_id)
            .max_by_key(|(_, record)| record.touched)
            .map(|(_, record)| record)
            .map(|record| record.snapshot.clone())
    }

    pub fn tool_record_count(&self) -> usize {
        self.tools.len()
    }

    pub fn message_record_count(&self) -> usize {
        self.messages.len()
    }

    pub fn message_batch_count(&self) -> usize {
        self.messages
            .values()
            .map(|record| record.batches.len())
            .sum()
    }

    pub fn message_accumulated_bytes(&self) -> usize {
        self.messages
            .values()
            .map(|record| record.accumulated_bytes)
            .sum()
    }

    pub fn apply_json(&mut self, body: &[u8], occurred_at_epoch_ms: u64) -> ClaudeReduceOutcome {
        let value: Value = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => return ClaudeReduceOutcome::malformed(),
        };
        let Some(event_name) = value.get("hook_event_name").and_then(Value::as_str) else {
            return ClaudeReduceOutcome::malformed();
        };

        self.event_clock = self.event_clock.wrapping_add(1);
        let occurrence = self.event_clock;

        match event_name {
            "SessionStart" => self.status(
                occurred_at_epoch_ms,
                "started",
                value.get("source").and_then(Value::as_str),
                None,
            ),
            "UserPromptSubmit" => {
                let deduplication_key =
                    self.official_deduplication_key(&value, "prompt_id", "claude-user-prompt");
                self.text_event(
                    occurred_at_epoch_ms,
                    value.get("prompt").and_then(Value::as_str),
                    |text| SemanticEventKind::UserMessage { text },
                    SemanticRetention::Canonical,
                    deduplication_key,
                )
            }
            "MessageDisplay" => self.message_display(&value, occurred_at_epoch_ms),
            "PreToolUse" => self.tool_event(
                &value,
                occurred_at_epoch_ms,
                SemanticToolState::Running,
                "running",
            ),
            "PostToolUse" => self.tool_event(
                &value,
                occurred_at_epoch_ms,
                SemanticToolState::Completed,
                "completed",
            ),
            "PostToolUseFailure" => self.tool_event(
                &value,
                occurred_at_epoch_ms,
                SemanticToolState::Failed,
                "failed",
            ),
            "PermissionRequest" => {
                self.permission_question(&value, occurred_at_epoch_ms, occurrence)
            }
            "PermissionDenied" => self.permission_denied(&value, occurred_at_epoch_ms),
            "Notification" => self.notification(&value, occurred_at_epoch_ms),
            "Elicitation" => self.elicitation(&value, occurred_at_epoch_ms, occurrence),
            "ElicitationResult" => self.status(
                occurred_at_epoch_ms,
                "questionAnswered",
                value.get("action").and_then(Value::as_str),
                self.official_deduplication_key(
                    &value,
                    "elicitation_id",
                    "claude-elicitation-result",
                ),
            ),
            "Stop" => self.stop(occurred_at_epoch_ms),
            "StopFailure" => self.stop_failure(&value, occurred_at_epoch_ms),
            "SessionEnd" => self.status(
                occurred_at_epoch_ms,
                "ended",
                value.get("reason").and_then(Value::as_str),
                None,
            ),
            "PostToolBatch" => self.status(occurred_at_epoch_ms, "toolsCompleted", None, None),
            "SubagentStart" | "SubagentStop" | "TaskCreated" | "TaskCompleted" | "PreCompact"
            | "PostCompact" => self.lifecycle_status(event_name, &value, occurred_at_epoch_ms),
            _ => ClaudeReduceOutcome::ignored(),
        }
    }

    fn draft(
        &self,
        occurred_at_epoch_ms: u64,
        kind: SemanticEventKind,
        retention: SemanticRetention,
        deduplication_key: Option<String>,
    ) -> SemanticEventDraft {
        SemanticEventDraft {
            stable_session_key: self.stable_session_key.clone(),
            occurred_at_epoch_ms,
            source: SemanticSource::Claude,
            kind,
            retention,
            deduplication_key,
        }
    }

    fn status(
        &self,
        occurred_at_epoch_ms: u64,
        state: &str,
        detail: Option<&str>,
        deduplication_key: Option<String>,
    ) -> ClaudeReduceOutcome {
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::Status {
                    state: state.to_string(),
                    detail: detail.map(bounded_text),
                },
                SemanticRetention::Canonical,
                deduplication_key,
            )],
            degraded: false,
        }
    }

    fn text_event(
        &self,
        occurred_at_epoch_ms: u64,
        text: Option<&str>,
        kind: impl FnOnce(String) -> SemanticEventKind,
        retention: SemanticRetention,
        deduplication_key: Option<String>,
    ) -> ClaudeReduceOutcome {
        let Some(text) = text.filter(|text| !text.is_empty()) else {
            return ClaudeReduceOutcome::malformed();
        };
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                kind(bounded_text(text)),
                retention,
                deduplication_key,
            )],
            degraded: false,
        }
    }

    fn tool_event(
        &mut self,
        value: &Value,
        occurred_at_epoch_ms: u64,
        requested_state: SemanticToolState,
        state_label: &str,
    ) -> ClaudeReduceOutcome {
        let Some(tool_use_id) = value.get("tool_use_id").and_then(Value::as_str) else {
            return ClaudeReduceOutcome::malformed();
        };
        let Some(name) = value.get("tool_name").and_then(Value::as_str) else {
            return ClaudeReduceOutcome::malformed();
        };
        let provider_session_id = self.provider_session_id(value);
        let tool_use_id = bounded_identifier(tool_use_id);
        let name = bounded_identifier(name);
        if tool_use_id.is_empty() || name.is_empty() {
            return ClaudeReduceOutcome::malformed();
        }

        self.tool_clock = self.tool_clock.wrapping_add(1);
        let mut changed = false;
        let key = ToolKey {
            provider_session_id: provider_session_id.clone(),
            tool_use_id: tool_use_id.clone(),
        };
        let record = self.tools.entry(key).or_insert_with(|| {
            changed = true;
            ToolRecord {
                snapshot: ClaudeToolSnapshot {
                    tool_use_id: tool_use_id.clone(),
                    name: name.clone(),
                    state: requested_state,
                },
                touched: self.tool_clock,
            }
        });
        record.touched = self.tool_clock;
        if record.snapshot.name != name {
            record.snapshot.name = name.clone();
            changed = true;
        }
        if should_advance_tool_state(record.snapshot.state, requested_state) {
            record.snapshot.state = requested_state;
            changed = true;
        }
        let state = record.snapshot.state;
        let summary_state = match state {
            SemanticToolState::Pending => "pending",
            SemanticToolState::Running => "running",
            SemanticToolState::Completed => "completed",
            SemanticToolState::Failed => "failed",
        };
        let summary_state = if state == requested_state {
            state_label
        } else {
            summary_state
        };
        let summary = format!("{} {summary_state}", record.snapshot.name);
        let snapshot_name = record.snapshot.name.clone();
        self.enforce_tool_limit();

        if !changed {
            return ClaudeReduceOutcome::ignored();
        }
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::Tool {
                    tool_id: tool_use_id.clone(),
                    name: snapshot_name,
                    state,
                    summary,
                },
                SemanticRetention::Canonical,
                Some(scoped_deduplication_key(
                    "claude-tool",
                    &provider_session_id,
                    &tool_use_id,
                )),
            )],
            degraded: false,
        }
    }

    fn message_display(&mut self, value: &Value, occurred_at_epoch_ms: u64) -> ClaudeReduceOutcome {
        let Some(turn_id) = value
            .get("turn_id")
            .and_then(Value::as_str)
            .map(bounded_identifier)
            .filter(|id| !id.is_empty())
        else {
            return ClaudeReduceOutcome::malformed();
        };
        let Some(message_id) = value
            .get("message_id")
            .and_then(Value::as_str)
            .map(bounded_identifier)
            .filter(|id| !id.is_empty())
        else {
            return ClaudeReduceOutcome::malformed();
        };
        let Some(index) = value.get("index").and_then(Value::as_u64) else {
            return ClaudeReduceOutcome::malformed();
        };
        let Some(final_chunk) = value.get("final").and_then(Value::as_bool) else {
            return ClaudeReduceOutcome::malformed();
        };
        let Some(delta) = value.get("delta").and_then(Value::as_str) else {
            return ClaudeReduceOutcome::malformed();
        };
        if self.limits.max_message_records == 0 || self.limits.max_message_batches_per_record == 0 {
            return ClaudeReduceOutcome::ignored();
        }

        let provider_session_id = self.provider_session_id(value);
        let key = MessageKey {
            provider_session_id: provider_session_id.clone(),
            turn_id,
            message_id,
        };
        self.message_clock = self.message_clock.wrapping_add(1);
        if !self.messages.contains_key(&key) {
            self.evict_oldest_message_if_full();
            self.messages.insert(
                key.clone(),
                MessageRecord {
                    batches: BTreeMap::new(),
                    next_index: 0,
                    text: String::new(),
                    finalized: false,
                    truncated: false,
                    accumulated_bytes: 0,
                    touched: self.message_clock,
                },
            );
        }

        let record = self.messages.get_mut(&key).expect("message inserted");
        record.touched = self.message_clock;
        if record.finalized
            || record.batches.contains_key(&index)
            || record.batches.len() >= self.limits.max_message_batches_per_record
        {
            return ClaudeReduceOutcome::ignored();
        }

        let remaining = self
            .limits
            .max_message_accumulated_bytes
            .saturating_sub(record.accumulated_bytes);
        let bounded_delta = utf8_prefix_by_bytes(delta, remaining);
        if bounded_delta.len() < delta.len() {
            record.truncated = true;
        }
        record.accumulated_bytes = record.accumulated_bytes.saturating_add(bounded_delta.len());
        record.batches.insert(
            index,
            MessageBatch {
                delta: bounded_delta.to_string(),
                final_chunk,
            },
        );

        let mut advanced = false;
        while let Some(batch) = record.batches.get_mut(&record.next_index) {
            let delta = std::mem::take(&mut batch.delta);
            record.text.push_str(&delta);
            advanced = true;
            record.next_index = record.next_index.saturating_add(1);
            if batch.final_chunk {
                record.finalized = true;
                break;
            }
        }
        if !advanced || record.text.is_empty() {
            return ClaudeReduceOutcome::ignored();
        }

        let text = if record.truncated {
            format!("{}\n[truncated by DevManager]", record.text)
        } else {
            record.text.clone()
        };
        let streaming = !record.finalized;
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::AssistantMessage {
                    message_id: key.message_id.clone(),
                    text: bounded_text(&text),
                    streaming,
                },
                SemanticRetention::Canonical,
                Some(scoped_message_deduplication_key(
                    "claude-message",
                    &provider_session_id,
                    &key.turn_id,
                    &key.message_id,
                )),
            )],
            degraded: false,
        }
    }

    fn permission_question(
        &self,
        value: &Value,
        occurred_at_epoch_ms: u64,
        occurrence: u64,
    ) -> ClaudeReduceOutcome {
        let Some(tool_name) = value.get("tool_name").and_then(Value::as_str) else {
            return ClaudeReduceOutcome::malformed();
        };
        let question_id = format!("permission-{occurrence}");
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::Question {
                    question_id: question_id.clone(),
                    prompt: format!(
                        "Claude requests permission to use {}",
                        bounded_identifier(tool_name)
                    ),
                    choices: Vec::new(),
                },
                SemanticRetention::Canonical,
                None,
            )],
            degraded: false,
        }
    }

    fn permission_denied(
        &mut self,
        value: &Value,
        occurred_at_epoch_ms: u64,
    ) -> ClaudeReduceOutcome {
        if value.get("tool_use_id").and_then(Value::as_str).is_some() {
            return self.tool_event(
                value,
                occurred_at_epoch_ms,
                SemanticToolState::Failed,
                "denied",
            );
        }
        let name = value
            .get("tool_name")
            .and_then(Value::as_str)
            .map(bounded_identifier)
            .unwrap_or_else(|| "tool".to_string());
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::Error {
                    message: format!("Permission denied for {name}"),
                },
                SemanticRetention::Canonical,
                None,
            )],
            degraded: false,
        }
    }

    fn notification(&self, value: &Value, occurred_at_epoch_ms: u64) -> ClaudeReduceOutcome {
        let notification_type = value
            .get("notification_type")
            .and_then(Value::as_str)
            .unwrap_or("notification");
        let detail = value
            .get("message")
            .and_then(Value::as_str)
            .map(bounded_text);
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::Status {
                    state: format!("notification:{}", bounded_identifier(notification_type)),
                    detail,
                },
                SemanticRetention::Canonical,
                None,
            )],
            degraded: false,
        }
    }

    fn elicitation(
        &self,
        value: &Value,
        occurred_at_epoch_ms: u64,
        occurrence: u64,
    ) -> ClaudeReduceOutcome {
        let Some(message) = value.get("message").and_then(Value::as_str) else {
            return ClaudeReduceOutcome::malformed();
        };
        let official_id = official_identifier(value, "elicitation_id");
        let question_id = official_id
            .clone()
            .unwrap_or_else(|| format!("elicitation-{occurrence}"));
        let deduplication_key = official_id.as_ref().map(|id| {
            scoped_deduplication_key("claude-elicitation", &self.provider_session_id(value), id)
        });
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::Question {
                    question_id: question_id.clone(),
                    prompt: bounded_text(message),
                    choices: Vec::new(),
                },
                SemanticRetention::Canonical,
                deduplication_key,
            )],
            degraded: false,
        }
    }

    fn stop(&self, occurred_at_epoch_ms: u64) -> ClaudeReduceOutcome {
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::Status {
                    state: "ready".to_string(),
                    detail: None,
                },
                SemanticRetention::Canonical,
                None,
            )],
            degraded: false,
        }
    }

    fn stop_failure(&self, value: &Value, occurred_at_epoch_ms: u64) -> ClaudeReduceOutcome {
        let Some(error) = value.get("error").and_then(Value::as_str) else {
            return ClaudeReduceOutcome::malformed();
        };
        let error = safe_stop_failure_category(error);
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::Error {
                    message: format!("Claude turn failed: {error}"),
                },
                SemanticRetention::Canonical,
                None,
            )],
            degraded: false,
        }
    }

    fn lifecycle_status(
        &self,
        event_name: &str,
        value: &Value,
        occurred_at_epoch_ms: u64,
    ) -> ClaudeReduceOutcome {
        let state = match event_name {
            "SubagentStart" => "subagentStarted",
            "SubagentStop" => "subagentStopped",
            "TaskCreated" => "taskCreated",
            "TaskCompleted" => "taskCompleted",
            "PreCompact" => "compacting",
            "PostCompact" => "compacted",
            _ => return ClaudeReduceOutcome::ignored(),
        };
        let detail = ["agent_type", "task_subject", "trigger"]
            .into_iter()
            .find_map(|field| value.get(field).and_then(Value::as_str))
            .map(bounded_text);
        let identity_field = match event_name {
            "SubagentStart" | "SubagentStop" => Some("agent_id"),
            "TaskCreated" | "TaskCompleted" => Some("task_id"),
            _ => None,
        };
        let deduplication_key = identity_field.and_then(|field| {
            self.official_deduplication_key(value, field, &format!("claude-{event_name}"))
        });
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::Status {
                    state: state.to_string(),
                    detail,
                },
                SemanticRetention::Canonical,
                deduplication_key,
            )],
            degraded: false,
        }
    }

    fn enforce_tool_limit(&mut self) {
        let limit = self.limits.max_tool_records;
        while self.tools.len() > limit {
            let Some(oldest) = self
                .tools
                .iter()
                .min_by_key(|(_, record)| record.touched)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.tools.remove(&oldest);
        }
    }

    fn evict_oldest_message_if_full(&mut self) {
        while self.messages.len() >= self.limits.max_message_records {
            let Some(oldest) = self
                .messages
                .iter()
                .min_by_key(|(_, record)| record.touched)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.messages.remove(&oldest);
        }
    }

    fn provider_session_id(&self, value: &Value) -> String {
        official_identifier(value, "session_id")
            .unwrap_or_else(|| self.fallback_provider_session_id.clone())
    }

    fn official_deduplication_key(
        &self,
        value: &Value,
        field: &str,
        prefix: &str,
    ) -> Option<String> {
        official_identifier(value, field)
            .map(|id| scoped_deduplication_key(prefix, &self.provider_session_id(value), &id))
    }
}

fn safe_stop_failure_category(error: &str) -> &'static str {
    match error {
        "rate_limit" => "rate_limit",
        "overloaded" => "overloaded",
        "authentication_failed" => "authentication_failed",
        "oauth_org_not_allowed" => "oauth_org_not_allowed",
        "billing_error" => "billing_error",
        "invalid_request" => "invalid_request",
        "model_not_found" => "model_not_found",
        "server_error" => "server_error",
        "max_output_tokens" => "max_output_tokens",
        "unknown" => "unknown",
        _ => "unknown",
    }
}

fn utf8_prefix_by_bytes(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn should_advance_tool_state(current: SemanticToolState, requested: SemanticToolState) -> bool {
    match (current, requested) {
        (SemanticToolState::Pending, _) => true,
        (SemanticToolState::Running, SemanticToolState::Completed | SemanticToolState::Failed) => {
            true
        }
        _ => false,
    }
}

fn official_identifier(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(bounded_identifier)
        .filter(|value| !value.is_empty())
}

fn scoped_deduplication_key(prefix: &str, provider_session_id: &str, id: &str) -> String {
    format!(
        "{prefix}:{}:{provider_session_id}:{}:{id}",
        provider_session_id.len(),
        id.len()
    )
}

fn scoped_message_deduplication_key(
    prefix: &str,
    provider_session_id: &str,
    turn_id: &str,
    message_id: &str,
) -> String {
    format!(
        "{prefix}:{}:{provider_session_id}:{}:{turn_id}:{}:{message_id}",
        provider_session_id.len(),
        turn_id.len(),
        message_id.len()
    )
}

fn bounded_identifier(value: &str) -> String {
    value.chars().take(256).collect()
}

fn bounded_text(value: &str) -> String {
    const TRUNCATION_SUFFIX: &str = "\n[truncated by DevManager]";
    let budget = MAX_PROVIDER_TEXT_BYTES.saturating_sub(TRUNCATION_SUFFIX.len() + 1);
    let mut raw_bytes = 0;
    let mut encoded_bytes = 0;
    for (index, character) in value.char_indices() {
        let next_raw = raw_bytes + character.len_utf8();
        let next_encoded = encoded_bytes + json_string_character_bytes(character);
        if next_raw > budget || next_encoded > budget {
            return format!("{}{TRUNCATION_SUFFIX}", &value[..index]);
        }
        raw_bytes = next_raw;
        encoded_bytes = next_encoded;
    }
    value.to_string()
}

fn json_string_character_bytes(character: char) -> usize {
    match character {
        '"' | '\\' | '\u{0008}' | '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' => 2,
        '\u{0000}'..='\u{001F}' => 6,
        _ => character.len_utf8(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeRegistryLimits {
    pub max_registrations: usize,
    pub max_body_bytes: usize,
    pub registration_ttl: Duration,
    pub reducer: ClaudeReducerLimits,
}

impl Default for ClaudeRegistryLimits {
    fn default() -> Self {
        Self {
            max_registrations: 128,
            max_body_bytes: MAX_CLAUDE_HOOK_BODY_BYTES,
            registration_ttl: Duration::from_secs(24 * 60 * 60),
            reducer: ClaudeReducerLimits::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeHookRegistration {
    pub nonce: String,
    pub stable_session_key: StableSessionKey,
    pub generation: u64,
}

struct RegisteredClaudeSession {
    stable_session_key: StableSessionKey,
    generation: u64,
    expires_at: Instant,
    activated: bool,
    reducer: ClaudeReducer,
    ingress_degraded: bool,
    cleanup_paths: Vec<PathBuf>,
}

struct ClaudeRegistryState {
    registrations: HashMap<String, RegisteredClaudeSession>,
    order: VecDeque<String>,
    next_generation: u64,
    latest_generation_by_key: HashMap<StableSessionKey, u64>,
}

pub struct ClaudeHookRegistry {
    limits: ClaudeRegistryLimits,
    publication_gate: RwLock<()>,
    ingress_generation_gate: RwLock<()>,
    state: Mutex<ClaudeRegistryState>,
    event_handler: RwLock<Option<ClaudeRegistryEventHandler>>,
}

struct ClaudeGenerationWriteGuards<'a> {
    _publication: std::sync::RwLockWriteGuard<'a, ()>,
    _ingress: std::sync::RwLockWriteGuard<'a, ()>,
}

pub type ClaudeRegistryEventHandler =
    Arc<dyn Fn(ClaudeHookRegistration, ClaudeRegistryEvent) + Send + Sync>;

#[derive(Debug, Clone)]
pub enum ClaudeRegistryEvent {
    Semantic(SemanticEventDraft),
    AdapterHealth {
        stable_session_key: StableSessionKey,
        health: SemanticAdapterHealth,
    },
    RegistrationDropped {
        stable_session_key: StableSessionKey,
        nonce: String,
        generation: u64,
        was_latest: bool,
    },
}

struct RemovedClaudeRegistration {
    nonce: String,
    stable_session_key: StableSessionKey,
    generation: u64,
    was_latest: bool,
    cleanup_paths: Vec<PathBuf>,
}

impl Default for ClaudeHookRegistry {
    fn default() -> Self {
        Self::with_limits(ClaudeRegistryLimits::default())
    }
}

impl ClaudeHookRegistry {
    pub fn with_limits(limits: ClaudeRegistryLimits) -> Self {
        Self {
            limits,
            publication_gate: RwLock::new(()),
            ingress_generation_gate: RwLock::new(()),
            state: Mutex::new(ClaudeRegistryState {
                registrations: HashMap::new(),
                order: VecDeque::new(),
                next_generation: 0,
                latest_generation_by_key: HashMap::new(),
            }),
            event_handler: RwLock::new(None),
        }
    }

    pub fn register_at(
        &self,
        stable_session_key: StableSessionKey,
        now: Instant,
    ) -> Result<ClaudeHookRegistration, String> {
        let publication_guard = self.lock_generation_write();
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Claude hook registry lock is poisoned".to_string())?;
        let mut removed = remove_expired(&mut state, now);
        while state.registrations.len() >= self.limits.max_registrations.max(1) {
            let Some(oldest) = state.order.pop_front() else {
                break;
            };
            if let Some(registration) = remove_registration(&mut state, &oldest) {
                removed.push(registration);
            }
        }

        let nonce = loop {
            let candidate = match random_nonce() {
                Ok(candidate) => candidate,
                Err(error) => {
                    drop(state);
                    drop(publication_guard);
                    self.finish_dropped_registrations(removed);
                    return Err(error);
                }
            };
            if !state.registrations.contains_key(&candidate) {
                break candidate;
            }
        };
        let Some(generation) = state.next_generation.checked_add(1) else {
            drop(state);
            drop(publication_guard);
            self.finish_dropped_registrations(removed);
            return Err("Claude hook registration generation exhausted".to_string());
        };
        state.next_generation = generation;
        state
            .latest_generation_by_key
            .insert(stable_session_key.clone(), generation);
        state.order.push_back(nonce.clone());
        state.registrations.insert(
            nonce.clone(),
            RegisteredClaudeSession {
                stable_session_key: stable_session_key.clone(),
                generation,
                expires_at: now + self.limits.registration_ttl.min(CLAUDE_ACTIVATION_GRACE),
                activated: false,
                reducer: ClaudeReducer::with_fallback_provider_session_id(
                    stable_session_key.clone(),
                    self.limits.reducer,
                    format!("registration-{generation}"),
                ),
                ingress_degraded: false,
                cleanup_paths: Vec::new(),
            },
        );
        let registration = ClaudeHookRegistration {
            nonce,
            stable_session_key,
            generation,
        };
        drop(state);
        drop(publication_guard);
        self.finish_dropped_registrations(removed);
        Ok(registration)
    }

    pub fn ingest_at(
        &self,
        peer: SocketAddr,
        nonce: &str,
        body: &[u8],
        now: Instant,
        occurred_at_epoch_ms: u64,
    ) -> RelayIngestStatus {
        self.ingest_captured_at(peer, nonce, body, now, occurred_at_epoch_ms)
            .status
    }

    fn ingest_captured_at(
        &self,
        peer: SocketAddr,
        nonce: &str,
        body: &[u8],
        now: Instant,
        occurred_at_epoch_ms: u64,
    ) -> CapturedClaudeIngest {
        let context = match self.admit_at(peer, nonce, body.len(), now) {
            Ok(context) => context,
            Err(status) => return CapturedClaudeIngest::without_session(status),
        };
        self.reduce_admitted(context, body, occurred_at_epoch_ms)
    }

    fn admit_at(
        &self,
        peer: SocketAddr,
        nonce: &str,
        body_len: usize,
        now: Instant,
    ) -> Result<ClaudeRegistrationContext, RelayIngestStatus> {
        if !peer.ip().is_loopback() {
            return Err(RelayIngestStatus::Rejected);
        }
        if body_len > self.limits.max_body_bytes {
            return Err(RelayIngestStatus::BodyTooLarge);
        }
        let publication_guard = self.lock_generation_write();
        let Ok(mut state) = self.state.lock() else {
            return Err(RelayIngestStatus::Rejected);
        };
        if state
            .registrations
            .get(nonce)
            .is_some_and(|registration| now > registration.expires_at)
        {
            state.order.retain(|candidate| candidate != nonce);
            let mut removed = remove_registration(&mut state, nonce)
                .map(|registration| vec![registration])
                .unwrap_or_default();
            removed.extend(remove_expired(&mut state, now));
            drop(state);
            drop(publication_guard);
            self.finish_dropped_registrations(removed);
            return Err(RelayIngestStatus::Expired);
        }
        let Some(registration) = state.registrations.get(nonce) else {
            let removed = remove_expired(&mut state, now);
            drop(state);
            drop(publication_guard);
            self.finish_dropped_registrations(removed);
            return Err(RelayIngestStatus::Rejected);
        };
        let context = ClaudeRegistrationContext {
            stable_session_key: registration.stable_session_key.clone(),
            nonce: nonce.to_string(),
            generation: registration.generation,
            admitted_at: now,
        };
        let registration = state
            .registrations
            .get_mut(nonce)
            .expect("registration checked above");
        if registration.activated {
            registration.expires_at = now + self.limits.registration_ttl;
        }
        Ok(context)
    }

    fn admit_ingress_at<T>(
        &self,
        peer: SocketAddr,
        nonce: &str,
        body_len: usize,
        now: Instant,
        enqueue: impl FnOnce(ClaudeRegistrationContext) -> T,
    ) -> Result<T, RelayIngestStatus> {
        if !peer.ip().is_loopback() {
            return Err(RelayIngestStatus::Rejected);
        }
        if body_len > self.limits.max_body_bytes {
            return Err(RelayIngestStatus::BodyTooLarge);
        }
        let _ingress_guard = self.lock_ingress_generation_read();
        let Ok(mut state) = self.state.lock() else {
            return Err(RelayIngestStatus::Rejected);
        };
        let Some(registration) = state.registrations.get(nonce) else {
            return Err(RelayIngestStatus::Rejected);
        };
        if now > registration.expires_at {
            return Err(RelayIngestStatus::Expired);
        }
        let context = ClaudeRegistrationContext {
            stable_session_key: registration.stable_session_key.clone(),
            nonce: nonce.to_string(),
            generation: registration.generation,
            admitted_at: now,
        };
        if !context_is_current(&state, &context) {
            return Err(RelayIngestStatus::Accepted(ClaudeReduceOutcome::ignored()));
        }
        let registration = state
            .registrations
            .get_mut(nonce)
            .expect("registration checked above");
        if registration.activated {
            registration.expires_at = now + self.limits.registration_ttl;
        }
        drop(state);
        Ok(enqueue(context))
    }

    fn reduce_admitted(
        &self,
        context: ClaudeRegistrationContext,
        body: &[u8],
        occurred_at_epoch_ms: u64,
    ) -> CapturedClaudeIngest {
        let Ok(mut state) = self.state.lock() else {
            return CapturedClaudeIngest::without_session(RelayIngestStatus::Rejected);
        };
        if !context_is_current(&state, &context) {
            return CapturedClaudeIngest {
                status: RelayIngestStatus::Accepted(ClaudeReduceOutcome::ignored()),
                context: Some(context),
                promoted_healthy: false,
            };
        }
        let is_session_start = serde_json::from_slice::<Value>(body)
            .ok()
            .is_some_and(|value| {
                value.get("hook_event_name").and_then(Value::as_str) == Some("SessionStart")
                    && official_identifier(&value, "session_id").is_some()
            });
        let registration = state
            .registrations
            .get_mut(&context.nonce)
            .expect("current registration exists");
        let outcome = registration.reducer.apply_json(body, occurred_at_epoch_ms);
        let promoted_healthy = is_session_start && !outcome.degraded && !registration.activated;
        if promoted_healthy {
            registration.activated = true;
            registration.expires_at = context.admitted_at + self.limits.registration_ttl;
        }
        CapturedClaudeIngest {
            status: RelayIngestStatus::Accepted(outcome),
            context: Some(context),
            promoted_healthy,
        }
    }

    pub fn set_event_handler(&self, handler: Option<ClaudeRegistryEventHandler>) {
        if let Ok(mut slot) = self.event_handler.write() {
            *slot = handler;
        }
    }

    pub fn attach_cleanup_path(&self, nonce: &str, path: PathBuf) -> bool {
        self.state
            .lock()
            .ok()
            .and_then(|mut state| {
                state.registrations.get_mut(nonce).map(|registration| {
                    registration.cleanup_paths.push(path);
                })
            })
            .is_some()
    }

    fn dispatch_captured(&self, captured: CapturedClaudeIngest) -> RelayIngestStatus {
        self.dispatch_captured_after_validation(captured, || {})
    }

    fn dispatch_captured_after_validation(
        &self,
        captured: CapturedClaudeIngest,
        before_publication: impl FnOnce(),
    ) -> RelayIngestStatus {
        let CapturedClaudeIngest {
            status,
            context,
            promoted_healthy,
        } = captured;
        let RelayIngestStatus::Accepted(outcome) = &status else {
            return status;
        };
        let Some(context) = context else {
            return status;
        };
        if !self.is_current_registration(&context) {
            return status;
        }
        let handler = self
            .event_handler
            .read()
            .ok()
            .and_then(|handler| handler.clone());
        before_publication();
        if let Some(handler) = handler.as_ref() {
            let registration = context.registration();
            for draft in &outcome.drafts {
                invoke_registry_handler(
                    handler,
                    registration.clone(),
                    ClaudeRegistryEvent::Semantic(draft.clone()),
                );
            }
            if outcome.degraded {
                invoke_registry_handler(
                    handler,
                    registration.clone(),
                    ClaudeRegistryEvent::AdapterHealth {
                        stable_session_key: context.stable_session_key.clone(),
                        health: SemanticAdapterHealth::Degraded,
                    },
                );
            }
            if promoted_healthy {
                invoke_registry_handler(
                    handler,
                    registration,
                    ClaudeRegistryEvent::AdapterHealth {
                        stable_session_key: context.stable_session_key.clone(),
                        health: SemanticAdapterHealth::Healthy,
                    },
                );
            }
        }
        status
    }

    pub fn unregister(&self, nonce: &str) -> Option<StableSessionKey> {
        let publication_guard = self.lock_generation_write();
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        let registration = remove_registration(&mut state, nonce);
        drop(state);
        drop(publication_guard);
        registration.map(|registration| {
            remove_cleanup_paths(registration.cleanup_paths);
            registration.stable_session_key
        })
    }

    pub(crate) fn unregister_registration(
        &self,
        expected: &ClaudeHookRegistration,
    ) -> Option<StableSessionKey> {
        let publication_guard = self.lock_generation_write();
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        let matches = state
            .registrations
            .get(&expected.nonce)
            .is_some_and(|registered| {
                registered.generation == expected.generation
                    && registered.stable_session_key == expected.stable_session_key
            });
        let registration = matches
            .then(|| remove_registration(&mut state, &expected.nonce))
            .flatten();
        drop(state);
        drop(publication_guard);
        registration.map(|registration| {
            remove_cleanup_paths(registration.cleanup_paths);
            registration.stable_session_key
        })
    }

    fn is_current_registration(&self, context: &ClaudeRegistrationContext) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| context_is_current(&state, context))
    }

    fn dispatch_degraded_if_current(&self, context: &ClaudeRegistrationContext) {
        let handler = self
            .event_handler
            .read()
            .ok()
            .and_then(|handler| handler.clone());
        if let Some(handler) = handler {
            invoke_registry_handler(
                &handler,
                context.registration(),
                ClaudeRegistryEvent::AdapterHealth {
                    stable_session_key: context.stable_session_key.clone(),
                    health: SemanticAdapterHealth::Degraded,
                },
            );
        }
    }

    fn mark_ingress_degraded(&self, context: &ClaudeRegistrationContext) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if !context_is_current(&state, context) {
            return false;
        }
        state
            .registrations
            .get_mut(&context.nonce)
            .map(|registration| registration.ingress_degraded = true)
            .is_some()
    }

    fn dispatch_pending_ingress_degradations(&self) {
        let contexts = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            let latest = state.latest_generation_by_key.clone();
            state
                .registrations
                .iter_mut()
                .filter_map(|(nonce, registration)| {
                    if !registration.ingress_degraded {
                        return None;
                    }
                    registration.ingress_degraded = false;
                    (latest.get(&registration.stable_session_key).copied()
                        == Some(registration.generation))
                    .then(|| ClaudeRegistrationContext {
                        stable_session_key: registration.stable_session_key.clone(),
                        nonce: nonce.clone(),
                        generation: registration.generation,
                        admitted_at: Instant::now(),
                    })
                })
                .collect::<Vec<_>>()
        };
        for context in contexts {
            self.dispatch_degraded_if_current(&context);
        }
    }

    pub(crate) fn publish_if_not_superseded(
        &self,
        stable_session_key: &StableSessionKey,
        generation: u64,
        publish: impl FnOnce(),
    ) -> bool {
        let _publication_guard = self.lock_generation_read();
        let Ok(state) = self.state.lock() else {
            return false;
        };
        if state
            .latest_generation_by_key
            .get(stable_session_key)
            .is_some_and(|latest| *latest > generation)
        {
            return false;
        }
        drop(state);
        publish();
        true
    }

    pub fn cleanup_expired_at(&self, now: Instant) -> usize {
        let publication_guard = self.lock_generation_write();
        let Ok(mut state) = self.state.lock() else {
            return 0;
        };
        let before = state.registrations.len();
        let removed_registrations = remove_expired(&mut state, now);
        let removed = before.saturating_sub(state.registrations.len());
        drop(state);
        drop(publication_guard);
        self.finish_dropped_registrations(removed_registrations);
        removed
    }

    pub fn registration_count(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.registrations.len())
            .unwrap_or(0)
    }

    pub fn max_body_bytes(&self) -> usize {
        self.limits.max_body_bytes
    }

    fn lock_generation_read(&self) -> std::sync::RwLockReadGuard<'_, ()> {
        match self.publication_gate.read() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let guard = poisoned.into_inner();
                self.publication_gate.clear_poison();
                guard
            }
        }
    }

    fn lock_ingress_generation_read(&self) -> std::sync::RwLockReadGuard<'_, ()> {
        match self.ingress_generation_gate.read() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let guard = poisoned.into_inner();
                self.ingress_generation_gate.clear_poison();
                guard
            }
        }
    }

    fn lock_generation_write(&self) -> ClaudeGenerationWriteGuards<'_> {
        let publication = match self.publication_gate.write() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let guard = poisoned.into_inner();
                self.publication_gate.clear_poison();
                guard
            }
        };
        let ingress = match self.ingress_generation_gate.write() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let guard = poisoned.into_inner();
                self.ingress_generation_gate.clear_poison();
                guard
            }
        };
        ClaudeGenerationWriteGuards {
            _publication: publication,
            _ingress: ingress,
        }
    }

    pub(crate) fn publish_if_current(
        &self,
        registration: &ClaudeHookRegistration,
        publish: impl FnOnce(),
    ) -> bool {
        let _publication_guard = self.lock_generation_read();
        let Ok(state) = self.state.lock() else {
            return false;
        };
        if !registration_is_current(&state, registration) {
            return false;
        }
        drop(state);
        publish();
        true
    }

    fn finish_dropped_registrations(&self, removed: Vec<RemovedClaudeRegistration>) {
        if removed.is_empty() {
            return;
        }
        let handler = self
            .event_handler
            .read()
            .ok()
            .and_then(|handler| handler.clone());
        for registration in removed {
            remove_cleanup_paths(registration.cleanup_paths);
            if let Some(handler) = handler.as_ref() {
                invoke_registry_handler(
                    handler,
                    ClaudeHookRegistration {
                        stable_session_key: registration.stable_session_key.clone(),
                        nonce: registration.nonce.clone(),
                        generation: registration.generation,
                    },
                    ClaudeRegistryEvent::RegistrationDropped {
                        stable_session_key: registration.stable_session_key,
                        nonce: registration.nonce,
                        generation: registration.generation,
                        was_latest: registration.was_latest,
                    },
                );
            }
        }
    }
}

fn invoke_registry_handler(
    handler: &ClaudeRegistryEventHandler,
    registration: ClaudeHookRegistration,
    event: ClaudeRegistryEvent,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handler(registration, event)
    }));
}

#[derive(Debug, Clone)]
pub enum RelayIngestStatus {
    Accepted(ClaudeReduceOutcome),
    Rejected,
    BodyTooLarge,
    Expired,
}

impl PartialEq for RelayIngestStatus {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Accepted(_), Self::Accepted(_))
                | (Self::Rejected, Self::Rejected)
                | (Self::BodyTooLarge, Self::BodyTooLarge)
                | (Self::Expired, Self::Expired)
        )
    }
}

impl Eq for RelayIngestStatus {}

struct CapturedClaudeIngest {
    status: RelayIngestStatus,
    context: Option<ClaudeRegistrationContext>,
    promoted_healthy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClaudeRegistrationContext {
    stable_session_key: StableSessionKey,
    nonce: String,
    generation: u64,
    admitted_at: Instant,
}

impl ClaudeRegistrationContext {
    fn registration(&self) -> ClaudeHookRegistration {
        ClaudeHookRegistration {
            nonce: self.nonce.clone(),
            stable_session_key: self.stable_session_key.clone(),
            generation: self.generation,
        }
    }
}

impl CapturedClaudeIngest {
    fn without_session(status: RelayIngestStatus) -> Self {
        Self {
            status,
            context: None,
            promoted_healthy: false,
        }
    }
}

fn context_is_current(state: &ClaudeRegistryState, context: &ClaudeRegistrationContext) -> bool {
    registration_is_current(state, &context.registration())
}

fn registration_is_current(
    state: &ClaudeRegistryState,
    registration: &ClaudeHookRegistration,
) -> bool {
    state
        .registrations
        .get(&registration.nonce)
        .is_some_and(|registered| {
            registered.generation == registration.generation
                && registered.stable_session_key == registration.stable_session_key
        })
        && state
            .latest_generation_by_key
            .get(&registration.stable_session_key)
            .copied()
            == Some(registration.generation)
}

fn remove_expired(state: &mut ClaudeRegistryState, now: Instant) -> Vec<RemovedClaudeRegistration> {
    let expired = state
        .registrations
        .iter()
        .filter(|(_, registration)| now > registration.expires_at)
        .map(|(nonce, _)| nonce.clone())
        .collect::<Vec<_>>();
    let mut removed = Vec::new();
    for nonce in expired {
        if let Some(registration) = remove_registration(state, &nonce) {
            removed.push(registration);
        }
    }
    state
        .order
        .retain(|nonce| state.registrations.contains_key(nonce));
    removed
}

fn remove_registration(
    state: &mut ClaudeRegistryState,
    nonce: &str,
) -> Option<RemovedClaudeRegistration> {
    state.order.retain(|candidate| candidate != nonce);
    let registration = state.registrations.remove(nonce)?;
    let was_latest = state
        .latest_generation_by_key
        .get(&registration.stable_session_key)
        .copied()
        == Some(registration.generation);
    if !state
        .registrations
        .values()
        .any(|candidate| candidate.stable_session_key == registration.stable_session_key)
    {
        state
            .latest_generation_by_key
            .remove(&registration.stable_session_key);
    }
    Some(RemovedClaudeRegistration {
        nonce: nonce.to_string(),
        stable_session_key: registration.stable_session_key,
        generation: registration.generation,
        was_latest,
        cleanup_paths: registration.cleanup_paths,
    })
}

fn remove_cleanup_paths(paths: Vec<PathBuf>) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeIngressLimits {
    pub max_critical_events: usize,
    pub max_optional_events: usize,
    pub max_critical_bytes: usize,
    pub max_optional_bytes: usize,
}

impl Default for ClaudeIngressLimits {
    fn default() -> Self {
        Self {
            max_critical_events: 256,
            max_optional_events: 64,
            max_critical_bytes: 4 * 1024 * 1024,
            max_optional_bytes: 1024 * 1024,
        }
    }
}

struct AdmittedClaudeHook {
    context: ClaudeRegistrationContext,
    body: Vec<u8>,
    occurred_at_epoch_ms: u64,
}

#[derive(Default)]
struct ClaudeIngressQueueState {
    critical: VecDeque<AdmittedClaudeHook>,
    optional: VecDeque<AdmittedClaudeHook>,
    critical_bytes: usize,
    optional_bytes: usize,
    degradation_pending: bool,
    shutdown: bool,
}

#[derive(Default)]
struct ClaudeIngressQueue {
    state: Mutex<ClaudeIngressQueueState>,
    ready: Condvar,
}

enum ClaudeIngressWork {
    Event(AdmittedClaudeHook),
    Degraded,
    Shutdown,
}

impl ClaudeIngressQueue {
    fn enqueue(
        &self,
        event: AdmittedClaudeHook,
        optional: bool,
        limits: ClaudeIngressLimits,
        registry: &ClaudeHookRegistry,
    ) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.shutdown {
            return;
        }
        let body_bytes = event.body.len();
        if optional {
            let full = state.optional.len() >= limits.max_optional_events
                || body_bytes
                    > limits
                        .max_optional_bytes
                        .saturating_sub(state.optional_bytes);
            if full {
                return;
            }
            state.optional_bytes = state.optional_bytes.saturating_add(body_bytes);
            state.optional.push_back(event);
        } else {
            let full = state.critical.len() >= limits.max_critical_events
                || body_bytes
                    > limits
                        .max_critical_bytes
                        .saturating_sub(state.critical_bytes);
            if full {
                let context = event.context;
                if registry.mark_ingress_degraded(&context) {
                    state.degradation_pending = true;
                    self.ready.notify_one();
                }
                return;
            }
            state.critical_bytes = state.critical_bytes.saturating_add(body_bytes);
            state.critical.push_back(event);
        }
        self.ready.notify_one();
    }

    fn next(&self) -> ClaudeIngressWork {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if state.shutdown {
                return ClaudeIngressWork::Shutdown;
            }
            if state.degradation_pending {
                state.degradation_pending = false;
                return ClaudeIngressWork::Degraded;
            }
            if let Some(event) = state.critical.pop_front() {
                state.critical_bytes = state.critical_bytes.saturating_sub(event.body.len());
                return ClaudeIngressWork::Event(event);
            }
            if let Some(event) = state.optional.pop_front() {
                state.optional_bytes = state.optional_bytes.saturating_sub(event.body.len());
                return ClaudeIngressWork::Event(event);
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn shutdown(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.shutdown = true;
        self.ready.notify_all();
    }
}

#[derive(Clone)]
struct ClaudeIngressState {
    registry: Arc<ClaudeHookRegistry>,
    queue: Arc<ClaudeIngressQueue>,
    limits: ClaudeIngressLimits,
}

pub struct ClaudeHookRelayListener {
    endpoint: String,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    server_thread: Option<thread::JoinHandle<()>>,
    queue: Arc<ClaudeIngressQueue>,
    consumer_thread: Option<thread::JoinHandle<()>>,
}

impl ClaudeHookRelayListener {
    pub fn start(registry: Arc<ClaudeHookRegistry>) -> Result<Self, String> {
        Self::start_with_ingress_limits(registry, ClaudeIngressLimits::default())
    }

    pub fn start_with_ingress_limits(
        registry: Arc<ClaudeHookRegistry>,
        limits: ClaudeIngressLimits,
    ) -> Result<Self, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| format!("bind Claude hook relay: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("configure Claude hook relay: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read Claude hook relay address: {error}"))?;
        let endpoint = format!("http://127.0.0.1:{}/internal/claude-hook", address.port());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("start Claude hook relay runtime: {error}"))?;
        let body_limit = registry.max_body_bytes();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let cleanup_registry = registry.clone();
        let queue = Arc::new(ClaudeIngressQueue::default());
        let consumer_queue = queue.clone();
        let consumer_registry = registry.clone();
        let consumer_thread = thread::Builder::new()
            .name("claude-hook-reducer".to_string())
            .spawn(move || loop {
                match consumer_queue.next() {
                    ClaudeIngressWork::Event(event) => {
                        let captured = consumer_registry.reduce_admitted(
                            event.context,
                            &event.body,
                            event.occurred_at_epoch_ms,
                        );
                        consumer_registry.dispatch_captured(captured);
                    }
                    ClaudeIngressWork::Degraded => {
                        consumer_registry.dispatch_pending_ingress_degradations();
                    }
                    ClaudeIngressWork::Shutdown => break,
                }
            })
            .map_err(|error| format!("spawn Claude hook reducer: {error}"))?;
        let ingress_state = ClaudeIngressState {
            registry,
            queue: queue.clone(),
            limits,
        };
        let server_thread_result = thread::Builder::new()
            .name("claude-hook-relay".to_string())
            .spawn(move || {
                runtime.block_on(async move {
                    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
                        return;
                    };
                    let app = Router::new()
                        .route("/internal/claude-hook", post(handle_claude_hook))
                        .layer(DefaultBodyLimit::max(body_limit))
                        .with_state(ingress_state);
                    let shutdown = async move {
                        let mut interval = tokio::time::interval(Duration::from_secs(60));
                        loop {
                            tokio::select! {
                                _ = interval.tick() => {
                                    cleanup_registry.cleanup_expired_at(Instant::now());
                                }
                                _ = &mut shutdown_rx => break,
                            }
                        }
                    };
                    let _ = axum::serve(
                        listener,
                        app.into_make_service_with_connect_info::<SocketAddr>(),
                    )
                    .with_graceful_shutdown(shutdown)
                    .await;
                });
            });
        let server_thread = match server_thread_result {
            Ok(thread) => thread,
            Err(error) => {
                queue.shutdown();
                let _ = consumer_thread.join();
                return Err(format!("spawn Claude hook relay: {error}"));
            }
        };
        Ok(Self {
            endpoint,
            shutdown_tx: Some(shutdown_tx),
            server_thread: Some(server_thread),
            queue,
            consumer_thread: Some(consumer_thread),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl Drop for ClaudeHookRelayListener {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(thread) = self.server_thread.take() {
            let _ = thread.join();
        }
        self.queue.shutdown();
        if let Some(thread) = self.consumer_thread.take() {
            let _ = thread.join();
        }
    }
}

async fn handle_claude_hook(
    State(ingress): State<ClaudeIngressState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let Some(nonce) = headers
        .get("x-devmanager-claude-nonce")
        .and_then(|value| value.to_str().ok())
    else {
        return StatusCode::UNAUTHORIZED;
    };
    let optional = is_optional_claude_hook(&body);
    let admission_registry = ingress.registry.clone();
    let queue_registry = ingress.registry.clone();
    let queue = ingress.queue.clone();
    let limits = ingress.limits;
    match admission_registry.admit_ingress_at(
        peer,
        nonce,
        body.len(),
        Instant::now(),
        move |context| {
            queue.enqueue(
                AdmittedClaudeHook {
                    context,
                    body: body.to_vec(),
                    occurred_at_epoch_ms: unix_epoch_ms(),
                },
                optional,
                limits,
                &queue_registry,
            );
        },
    ) {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(RelayIngestStatus::Rejected) => StatusCode::UNAUTHORIZED,
        Err(RelayIngestStatus::BodyTooLarge) => StatusCode::PAYLOAD_TOO_LARGE,
        Err(RelayIngestStatus::Expired) => StatusCode::GONE,
        Err(RelayIngestStatus::Accepted(_)) => StatusCode::NO_CONTENT,
    }
}

fn is_optional_claude_hook(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("hook_event_name")
                .and_then(Value::as_str)
                .map(|event| event == "MessageDisplay")
        })
        .unwrap_or(false)
}

fn unix_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn random_nonce() -> Result<String, String> {
    random_hex_token::<CLAUDE_NONCE_BYTES>("Claude hook nonce")
}

fn random_settings_token() -> Result<String, String> {
    random_hex_token::<CLAUDE_SETTINGS_TOKEN_BYTES>("Claude settings filename")
}

fn random_hex_token<const N: usize>(label: &str) -> Result<String, String> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).map_err(|error| format!("generate {label}: {error}"))?;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    Ok(encoded)
}

pub fn run_hook_relay(endpoint: &str, nonce: &str, body: &[u8]) -> ExitCode {
    if body.len() > MAX_CLAUDE_HOOK_BODY_BYTES || !is_valid_loopback_relay_url(endpoint) {
        return ExitCode::SUCCESS;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(125)))
        .max_redirects(0)
        .proxy(None)
        .build()
        .into();
    let _ = agent
        .post(endpoint)
        .header("content-type", "application/json")
        .header("x-devmanager-claude-nonce", nonce)
        .send(body);
    ExitCode::SUCCESS
}

pub fn run_hook_relay_subcommand<R: Read>(args: &[String], reader: R) -> Option<ExitCode> {
    if args.first().map(String::as_str) != Some("claude-hook-relay") {
        return None;
    }
    let [_, url_flag, endpoint, nonce_flag, nonce] = args else {
        return Some(ExitCode::SUCCESS);
    };
    if url_flag != "--url" || nonce_flag != "--nonce" {
        return Some(ExitCode::SUCCESS);
    }
    let mut body = Vec::new();
    let mut limited = reader.take((MAX_CLAUDE_HOOK_BODY_BYTES + 1) as u64);
    if limited.read_to_end(&mut body).is_err() || body.len() > MAX_CLAUDE_HOOK_BODY_BYTES {
        return Some(ExitCode::SUCCESS);
    }
    Some(run_hook_relay(endpoint, nonce, &body))
}

pub fn is_valid_loopback_relay_url(endpoint: &str) -> bool {
    // `http::Uri` intentionally discards a URI fragment because fragments are
    // not part of an HTTP request target. Reject it before parsing so the
    // accepted relay spelling remains exact and unambiguous.
    if endpoint.as_bytes().contains(&b'#') {
        return false;
    }
    let Ok(uri) = endpoint.parse::<ureq::http::Uri>() else {
        return false;
    };
    if uri.scheme_str() != Some("http") {
        return false;
    }
    let Some(authority) = uri.authority() else {
        return false;
    };
    if authority.as_str().contains('@') || authority.port_u16().is_none() {
        return false;
    }
    if !matches!(authority.host(), "127.0.0.1" | "[::1]") {
        return false;
    }
    let Some(path_and_query) = uri.path_and_query() else {
        return false;
    };
    path_and_query.path() == "/internal/claude-hook" && path_and_query.query().is_none()
}

const CLAUDE_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PermissionDenied",
    "PostToolUse",
    "PostToolUseFailure",
    "PostToolBatch",
    "Notification",
    "MessageDisplay",
    "Elicitation",
    "ElicitationResult",
    "SubagentStart",
    "SubagentStop",
    "TaskCreated",
    "TaskCompleted",
    "PreCompact",
    "PostCompact",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeShellKind {
    Posix,
    PowerShell,
    Cmd,
}

#[derive(Debug, Clone)]
pub struct ClaudeLaunchOverlay {
    pub startup_command: String,
    pub endpoint: String,
    pub registration: Option<ClaudeHookRegistration>,
    pub settings_path: Option<PathBuf>,
    pub health: SemanticAdapterHealth,
    pub diagnostic: Option<String>,
}

impl ClaudeLaunchOverlay {
    fn degraded(startup_command: &str, endpoint: &str, diagnostic: impl Into<String>) -> Self {
        Self {
            startup_command: startup_command.to_string(),
            endpoint: endpoint.to_string(),
            registration: None,
            settings_path: None,
            health: SemanticAdapterHealth::Degraded,
            diagnostic: Some(diagnostic.into()),
        }
    }
}

#[derive(Debug)]
struct ShellToken {
    value: String,
    start: usize,
    end: usize,
}

#[derive(Debug)]
struct SettingsArgument {
    value: String,
    start: usize,
    end: usize,
}

/// Creates an ephemeral Claude Code settings overlay for commands whose
/// executable identity can be established without evaluating a shell.
/// Unrecognized or ambiguous commands are returned byte-for-byte unchanged.
#[allow(clippy::too_many_arguments)]
pub fn prepare_claude_launch_overlay(
    registry: &ClaudeHookRegistry,
    stable_session_key: StableSessionKey,
    startup_command: &str,
    shell: ClaudeShellKind,
    devmanager_executable: &Path,
    endpoint: &str,
    temp_root: &Path,
    now: Instant,
) -> ClaudeLaunchOverlay {
    if !is_valid_loopback_relay_url(endpoint) {
        return ClaudeLaunchOverlay::degraded(
            startup_command,
            endpoint,
            "Claude hook relay endpoint is not an exact loopback URL",
        );
    }
    let tokens = match tokenize_shell_command(startup_command, shell) {
        Ok(tokens) => tokens,
        Err(error) => return ClaudeLaunchOverlay::degraded(startup_command, endpoint, error),
    };
    let argument_start = match claude_argument_start(&tokens) {
        Some(index) => index,
        None => {
            return ClaudeLaunchOverlay::degraded(
                startup_command,
                endpoint,
                "startup command is not a directly recognized Claude Code command",
            )
        }
    };
    let settings_argument = match find_settings_argument(&tokens, argument_start) {
        Ok(argument) => argument,
        Err(error) => return ClaudeLaunchOverlay::degraded(startup_command, endpoint, error),
    };
    let mut settings = match settings_argument.as_ref() {
        Some(argument) => match load_settings_value(&argument.value) {
            Ok(settings) => settings,
            Err(error) => return ClaudeLaunchOverlay::degraded(startup_command, endpoint, error),
        },
        None => Value::Object(serde_json::Map::new()),
    };
    if !settings.is_object() {
        return ClaudeLaunchOverlay::degraded(
            startup_command,
            endpoint,
            "Claude settings must be a JSON object",
        );
    }
    if shell == ClaudeShellKind::Cmd && !is_safe_cmd_settings_root(temp_root) {
        return ClaudeLaunchOverlay::degraded(
            startup_command,
            endpoint,
            "Claude settings overlay path cannot be quoted safely for cmd.exe",
        );
    }

    let registration = match registry.register_at(stable_session_key, now) {
        Ok(registration) => registration,
        Err(error) => return ClaudeLaunchOverlay::degraded(startup_command, endpoint, error),
    };
    if let Err(error) = merge_relay_hooks(
        &mut settings,
        devmanager_executable,
        endpoint,
        &registration.nonce,
    ) {
        registry.unregister(&registration.nonce);
        return ClaudeLaunchOverlay::degraded(startup_command, endpoint, error);
    }
    if let Err(error) = fs::create_dir_all(temp_root) {
        registry.unregister(&registration.nonce);
        return ClaudeLaunchOverlay::degraded(
            startup_command,
            endpoint,
            format!("create Claude settings overlay directory: {error}"),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(temp_root, fs::Permissions::from_mode(0o700)) {
            registry.unregister(&registration.nonce);
            return ClaudeLaunchOverlay::degraded(
                startup_command,
                endpoint,
                format!("secure Claude settings overlay directory: {error}"),
            );
        }
    }
    let settings_token = match random_settings_token() {
        Ok(token) => token,
        Err(error) => {
            registry.unregister(&registration.nonce);
            return ClaudeLaunchOverlay::degraded(startup_command, endpoint, error);
        }
    };
    let settings_path = temp_root.join(format!("claude-hooks-{settings_token}.json"));
    let encoded = match serde_json::to_vec_pretty(&settings) {
        Ok(encoded) => encoded,
        Err(error) => {
            registry.unregister(&registration.nonce);
            return ClaudeLaunchOverlay::degraded(
                startup_command,
                endpoint,
                format!("encode Claude settings overlay: {error}"),
            );
        }
    };
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let write_result = options
        .open(&settings_path)
        .and_then(|mut file| file.write_all(&encoded));
    if let Err(error) = write_result {
        registry.unregister(&registration.nonce);
        let _ = fs::remove_file(&settings_path);
        return ClaudeLaunchOverlay::degraded(
            startup_command,
            endpoint,
            format!("write Claude settings overlay: {error}"),
        );
    }
    if !registry.attach_cleanup_path(&registration.nonce, settings_path.clone()) {
        let _ = fs::remove_file(&settings_path);
        registry.unregister(&registration.nonce);
        return ClaudeLaunchOverlay::degraded(
            startup_command,
            endpoint,
            "Claude hook registration expired before its overlay was attached",
        );
    }

    let quoted_path = quote_shell_argument(&settings_path.to_string_lossy(), shell);
    let replacement = format!("--settings {quoted_path}");
    let startup_command = if let Some(argument) = settings_argument {
        format!(
            "{}{}{}",
            &startup_command[..argument.start],
            replacement,
            &startup_command[argument.end..]
        )
    } else {
        format!(
            "{}{}{}",
            startup_command,
            if startup_command.ends_with(char::is_whitespace) {
                ""
            } else {
                " "
            },
            replacement
        )
    };
    ClaudeLaunchOverlay {
        startup_command,
        endpoint: endpoint.to_string(),
        registration: Some(registration),
        settings_path: Some(settings_path),
        // Writing an overlay proves only that launch preparation succeeded.
        // The adapter becomes healthy after the matching Claude process calls
        // the relay with its current-generation SessionStart hook.
        health: SemanticAdapterHealth::Degraded,
        diagnostic: None,
    }
}

fn merge_relay_hooks(
    settings: &mut Value,
    devmanager_executable: &Path,
    endpoint: &str,
    nonce: &str,
) -> Result<(), String> {
    let settings = settings
        .as_object_mut()
        .ok_or_else(|| "Claude settings must be a JSON object".to_string())?;
    if !settings.contains_key("hooks") {
        settings.insert("hooks".to_string(), Value::Object(serde_json::Map::new()));
    }
    let hooks = settings
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "Claude settings hooks must be a JSON object".to_string())?;
    for event in CLAUDE_HOOK_EVENTS {
        if !hooks.contains_key(*event) {
            hooks.insert((*event).to_string(), Value::Array(Vec::new()));
        }
        let event_hooks = hooks
            .get_mut(*event)
            .and_then(Value::as_array_mut)
            .ok_or_else(|| format!("Claude settings hook {event} must be an array"))?;
        event_hooks.push(serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": devmanager_executable.display().to_string(),
                "args": ["claude-hook-relay", "--url", endpoint, "--nonce", nonce],
                "async": true
            }]
        }));
    }
    Ok(())
}

fn load_settings_value(argument: &str) -> Result<Value, String> {
    if argument.trim_start().starts_with('{') {
        if argument.len() > MAX_CLAUDE_SETTINGS_BYTES {
            return Err(format!(
                "inline Claude settings exceed the {} byte limit",
                MAX_CLAUDE_SETTINGS_BYTES
            ));
        }
        return serde_json::from_str(argument)
            .map_err(|error| format!("parse inline Claude settings: {error}"));
    }
    let file = fs::File::open(argument)
        .map_err(|error| format!("read existing Claude settings {}: {error}", argument))?;
    let mut bytes = Vec::new();
    file.take((MAX_CLAUDE_SETTINGS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read existing Claude settings {}: {error}", argument))?;
    if bytes.len() > MAX_CLAUDE_SETTINGS_BYTES {
        return Err(format!(
            "existing Claude settings {} exceed the {} byte limit",
            argument, MAX_CLAUDE_SETTINGS_BYTES
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse existing Claude settings {}: {error}", argument))
}

fn find_settings_argument(
    tokens: &[ShellToken],
    argument_start: usize,
) -> Result<Option<SettingsArgument>, String> {
    let mut found = None;
    let mut index = argument_start;
    while index < tokens.len() {
        let token = &tokens[index];
        let candidate = if token.value == "--settings" {
            let Some(value) = tokens.get(index + 1) else {
                return Err("Claude --settings requires a value".to_string());
            };
            index += 1;
            Some(SettingsArgument {
                value: value.value.clone(),
                start: token.start,
                end: value.end,
            })
        } else if let Some(value) = token.value.strip_prefix("--settings=") {
            if value.is_empty() {
                return Err("Claude --settings requires a value".to_string());
            }
            Some(SettingsArgument {
                value: value.to_string(),
                start: token.start,
                end: token.end,
            })
        } else {
            None
        };
        if let Some(candidate) = candidate {
            if found.is_some() {
                return Err("multiple Claude --settings arguments are ambiguous".to_string());
            }
            found = Some(candidate);
        }
        index += 1;
    }
    Ok(found)
}

fn claude_argument_start(tokens: &[ShellToken]) -> Option<usize> {
    let first = tokens.first()?;
    let executable = command_basename(&first.value);
    if matches!(
        executable.as_str(),
        "claude" | "claude.exe" | "claude.cmd" | "claude.ps1"
    ) {
        return Some(1);
    }
    if !matches!(executable.as_str(), "npx" | "npx.exe" | "npx.cmd") {
        return None;
    }
    let mut index = 1;
    while tokens
        .get(index)
        .is_some_and(|token| matches!(token.value.as_str(), "-y" | "--yes"))
    {
        index += 1;
    }
    let package = tokens.get(index)?.value.as_str();
    let suffix = package.strip_prefix("@anthropic-ai/claude-code")?;
    if !suffix.is_empty() && !(suffix.starts_with('@') && suffix.len() > 1) {
        return None;
    }
    Some(index + 1)
}

fn command_basename(value: &str) -> String {
    value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase()
}

fn tokenize_shell_command(
    command: &str,
    shell: ClaudeShellKind,
) -> Result<Vec<ShellToken>, String> {
    let mut chars = command.char_indices().peekable();
    let mut tokens = Vec::new();
    while let Some(&(index, ch)) = chars.peek() {
        if matches!(ch, '\r' | '\n') {
            return Err(
                "multi-command shell input is not eligible for Claude hook injection".to_string(),
            );
        }
        if (ch == '#' && shell != ClaudeShellKind::Cmd)
            || (ch == '`' && shell == ClaudeShellKind::Posix)
        {
            return Err(
                "shell comments or substitutions are not eligible for Claude hook injection"
                    .to_string(),
            );
        }
        if ch.is_whitespace() {
            chars.next();
            continue;
        }
        let start = index;
        let mut value = String::new();
        let mut quote = None;
        let mut end = command.len();
        while let Some(&(index, ch)) = chars.peek() {
            if let Some(delimiter) = quote {
                chars.next();
                if matches!(ch, '\r' | '\n') {
                    return Err(
                        "multi-command shell input is not eligible for Claude hook injection"
                            .to_string(),
                    );
                }
                if ch == '`' && shell == ClaudeShellKind::Posix && delimiter != '\'' {
                    return Err(
                        "shell comments or substitutions are not eligible for Claude hook injection"
                            .to_string(),
                    );
                }
                if ch == delimiter {
                    if shell == ClaudeShellKind::PowerShell
                        && delimiter == '\''
                        && chars.peek().is_some_and(|(_, next)| *next == '\'')
                    {
                        chars.next();
                        value.push('\'');
                    } else {
                        quote = None;
                    }
                    continue;
                }
                if (shell == ClaudeShellKind::Posix && delimiter == '"' && ch == '\\')
                    || (shell == ClaudeShellKind::PowerShell && ch == '`')
                    || (shell == ClaudeShellKind::Cmd && ch == '^')
                {
                    let Some((_, escaped)) = chars.next() else {
                        return Err("unterminated shell escape".to_string());
                    };
                    value.push(escaped);
                } else {
                    value.push(ch);
                }
                continue;
            }
            if matches!(ch, '\r' | '\n') {
                return Err(
                    "multi-command shell input is not eligible for Claude hook injection"
                        .to_string(),
                );
            }
            if (ch == '#' && shell != ClaudeShellKind::Cmd)
                || (ch == '`' && shell == ClaudeShellKind::Posix)
            {
                return Err(
                    "shell comments or substitutions are not eligible for Claude hook injection"
                        .to_string(),
                );
            }
            if ch.is_whitespace() {
                end = index;
                break;
            }
            if matches!(ch, '|' | '&' | ';' | '<' | '>' | '\r' | '\n' | '(' | ')') {
                return Err(
                    "shell operators are not eligible for Claude hook injection".to_string()
                );
            }
            chars.next();
            if ch == '"' || (ch == '\'' && shell != ClaudeShellKind::Cmd) {
                quote = Some(ch);
            } else if (shell == ClaudeShellKind::Posix && ch == '\\')
                || (shell == ClaudeShellKind::PowerShell && ch == '`')
                || (shell == ClaudeShellKind::Cmd && ch == '^')
            {
                let Some((_, escaped)) = chars.next() else {
                    return Err("unterminated shell escape".to_string());
                };
                value.push(escaped);
            } else {
                value.push(ch);
            }
        }
        if quote.is_some() {
            return Err("unterminated shell quote".to_string());
        }
        if value.is_empty() {
            return Err("empty shell token is not eligible for Claude hook injection".to_string());
        }
        tokens.push(ShellToken { value, start, end });
    }
    if tokens.is_empty() {
        return Err("empty startup command".to_string());
    }
    Ok(tokens)
}

pub fn quote_shell_argument(value: &str, shell: ClaudeShellKind) -> String {
    match shell {
        ClaudeShellKind::Posix => format!("'{}'", value.replace('\'', "'\\''")),
        ClaudeShellKind::PowerShell => format!("'{}'", value.replace('\'', "''")),
        ClaudeShellKind::Cmd => format!("\"{}\"", value.replace('"', "\"\"")),
    }
}

/// Appends provider-owned Claude CLI arguments only after proving that the
/// configured command is a single, directly recognized Claude invocation.
pub fn append_claude_cli_arguments(
    startup_command: &str,
    shell: ClaudeShellKind,
    arguments: &[String],
) -> Result<String, String> {
    let tokens = tokenize_shell_command(startup_command, shell)?;
    if claude_argument_start(&tokens).is_none() {
        return Err("startup command is not a directly recognized Claude Code command".to_string());
    }
    if arguments.is_empty() {
        return Ok(startup_command.to_string());
    }
    if arguments
        .iter()
        .any(|argument| argument.is_empty() || argument.contains(['\r', '\n']))
    {
        return Err("Claude provider arguments must be nonblank single-line values".to_string());
    }
    let mut command = startup_command.to_string();
    if !command.ends_with(char::is_whitespace) {
        command.push(' ');
    }
    command.push_str(
        &arguments
            .iter()
            .map(|argument| {
                if argument.starts_with("--")
                    && argument[2..]
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '-')
                {
                    argument.clone()
                } else {
                    quote_shell_argument(argument, shell)
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    );
    Ok(command)
}

fn is_safe_cmd_settings_root(path: &Path) -> bool {
    path.to_str().is_some_and(|value| {
        !value
            .chars()
            .any(|character| matches!(character, '%' | '!' | '"' | '\r' | '\n'))
    })
}

#[cfg(test)]
mod registry_race_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn accepted_old_ingest_cannot_dispatch_after_replacement_registration() {
        let registry = ClaudeHookRegistry::default();
        let registration = registry
            .register_at(StableSessionKey::from_tab("race-tab"), Instant::now())
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        registry.set_event_handler(Some(Arc::new(move |_registration, event| {
            observed.lock().unwrap().push(event);
        })));
        let captured = registry.ingest_captured_at(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45000),
            &registration.nonce,
            br#"{"hook_event_name":"UserPromptSubmit","prompt":"race"}"#,
            Instant::now(),
            1_800_000_000_000,
        );

        // Models a replacement winning between relay admission and reducer
        // dispatch. The accepted old event must not update the shared key.
        let replacement = registry
            .register_at(StableSessionKey::from_tab("race-tab"), Instant::now())
            .unwrap();
        assert!(replacement.generation > registration.generation);
        let status = registry.dispatch_captured(captured);

        assert!(matches!(status, RelayIngestStatus::Accepted(_)));
        assert!(!events.lock().unwrap().iter().any(|event| matches!(
            event,
            ClaudeRegistryEvent::Semantic(SemanticEventDraft {
                stable_session_key,
                kind: SemanticEventKind::UserMessage { text },
                ..
            }) if stable_session_key == &StableSessionKey::from_tab("race-tab") && text == "race"
        )));
    }

    #[test]
    fn replacement_while_old_dispatch_is_paused_blocks_old_draft_and_health_publication() {
        for (body, label) in [
            (
                &br#"{"hook_event_name":"UserPromptSubmit","prompt":"stale draft"}"#[..],
                "draft",
            ),
            (&br#"{"hook_event_name":"PreToolUse""#[..], "adapter health"),
            (
                &br#"{"hook_event_name":"SessionStart","session_id":"stale-session","source":"startup"}"#[..],
                "healthy promotion",
            ),
        ] {
            let registry = Arc::new(ClaudeHookRegistry::default());
            let old = registry
                .register_at(StableSessionKey::from_tab("race-tab"), Instant::now())
                .unwrap();
            let events = Arc::new(Mutex::new(Vec::new()));
            let observed = events.clone();
            let publication_registry = registry.clone();
            registry.set_event_handler(Some(Arc::new(move |registration, event| {
                publication_registry.publish_if_current(&registration, || {
                    observed.lock().unwrap().push(event);
                });
            })));
            let captured = registry.ingest_captured_at(
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45000),
                &old.nonce,
                body,
                Instant::now(),
                1_800_000_000_000,
            );
            let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
            let dispatch_registry = registry.clone();
            let dispatch_gate = gate.clone();
            let dispatch = thread::spawn(move || {
                dispatch_registry.dispatch_captured_after_validation(captured, move || {
                    let (lock, condition) = &*dispatch_gate;
                    let mut state = lock.lock().unwrap();
                    state.0 = true;
                    condition.notify_all();
                    while !state.1 {
                        state = condition.wait(state).unwrap();
                    }
                })
            });

            {
                let (lock, condition) = &*gate;
                let state = lock.lock().unwrap();
                let (state, timeout) = condition
                    .wait_timeout_while(state, Duration::from_secs(2), |state| !state.0)
                    .unwrap();
                assert!(!timeout.timed_out(), "old {label} dispatch never paused");
                drop(state);
            }
            let replacement = registry
                .register_at(StableSessionKey::from_tab("race-tab"), Instant::now())
                .unwrap();
            assert!(replacement.generation > old.generation);
            {
                let (lock, condition) = &*gate;
                let mut state = lock.lock().unwrap();
                state.1 = true;
                condition.notify_all();
            }
            assert!(matches!(
                dispatch.join().unwrap(),
                RelayIngestStatus::Accepted(_)
            ));
            assert!(
                events.lock().unwrap().is_empty(),
                "superseded {label} reached the publisher"
            );
        }
    }
}
