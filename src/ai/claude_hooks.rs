use crate::domain::{
    AgentSessionId, ProviderSessionId, ResourceId, TaskId, MAX_PROVIDER_SESSION_ID_BYTES,
};
use crate::remote::presentation::{
    SemanticAdapterHealth, SemanticEventDraft, SemanticEventKind, SemanticRetention,
    SemanticSource, SemanticToolState, StableSessionKey,
};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;

pub const MAX_CLAUDE_HOOK_BODY_BYTES: usize = 256 * 1024;
pub const MAX_CLAUDE_HOOK_JSON_NESTING: usize = 8;
pub const MAX_CLAUDE_HOOK_JSON_STRING_BYTES: usize = 4096;
pub const MAX_CLAUDE_HOOK_JSON_MAP_ENTRIES: usize = 32;
pub const MAX_CLAUDE_HOOK_JSON_ARRAY_ELEMENTS: usize = 32;
pub const MAX_CLAUDE_HOOK_JSON_TOTAL_NODES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeHookJsonBound {
    BodyTooLarge,
    Invalid,
}

pub fn physically_bound_claude_hook_json(body: &[u8]) -> Result<(), ClaudeHookJsonBound> {
    if body.len() > MAX_CLAUDE_HOOK_BODY_BYTES {
        return Err(ClaudeHookJsonBound::BodyTooLarge);
    }
    scan_claude_hook_json_bounds(body)
}

fn scan_claude_hook_json_bounds(body: &[u8]) -> Result<(), ClaudeHookJsonBound> {
    let mut depth = 0_usize;
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut string_bytes = 0_usize;
    let mut total_nodes = 0_usize;
    let mut object_entries = [0_usize; MAX_CLAUDE_HOOK_JSON_NESTING + 1];
    let mut array_elements = [0_usize; MAX_CLAUDE_HOOK_JSON_NESTING + 1];
    let mut expecting_key = [false; MAX_CLAUDE_HOOK_JSON_NESTING + 1];
    let mut in_array = [false; MAX_CLAUDE_HOOK_JSON_NESTING + 1];
    let mut awaiting_array_value = [false; MAX_CLAUDE_HOOK_JSON_NESTING + 1];
    while index < body.len() {
        let byte = body[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
                if depth > 0 && expecting_key[depth] {
                    object_entries[depth] = object_entries[depth].saturating_add(1);
                    if object_entries[depth] > MAX_CLAUDE_HOOK_JSON_MAP_ENTRIES {
                        return Err(ClaudeHookJsonBound::Invalid);
                    }
                    expecting_key[depth] = false;
                }
            }
            string_bytes = string_bytes.saturating_add(1);
            if string_bytes > MAX_CLAUDE_HOOK_JSON_STRING_BYTES {
                return Err(ClaudeHookJsonBound::Invalid);
            }
            index += 1;
            continue;
        }
        match byte {
            b' ' | b'\n' | b'\r' | b'\t' => {}
            b'"' => {
                count_hook_json_node(&mut total_nodes)?;
                note_array_element(
                    &mut array_elements,
                    &mut awaiting_array_value,
                    depth,
                    in_array,
                )?;
                in_string = true;
                string_bytes = 0;
            }
            b'{' | b'[' => {
                count_hook_json_node(&mut total_nodes)?;
                note_array_element(
                    &mut array_elements,
                    &mut awaiting_array_value,
                    depth,
                    in_array,
                )?;
                depth = depth.saturating_add(1);
                if depth > MAX_CLAUDE_HOOK_JSON_NESTING {
                    return Err(ClaudeHookJsonBound::Invalid);
                }
                object_entries[depth] = 0;
                array_elements[depth] = 0;
                expecting_key[depth] = byte == b'{';
                in_array[depth] = byte == b'[';
                awaiting_array_value[depth] = byte == b'[';
            }
            b'}' | b']' => {
                if depth == 0 {
                    return Err(ClaudeHookJsonBound::Invalid);
                }
                depth -= 1;
            }
            b',' => {
                if depth > 0 {
                    expecting_key[depth] = !in_array[depth];
                    awaiting_array_value[depth] = in_array[depth];
                }
            }
            b':' => {}
            b't' | b'f' | b'n' | b'-' | b'0'..=b'9' => {
                count_hook_json_node(&mut total_nodes)?;
                note_array_element(
                    &mut array_elements,
                    &mut awaiting_array_value,
                    depth,
                    in_array,
                )?;
                while index + 1 < body.len() {
                    let next = body[index + 1];
                    if next == b',' || next == b'}' || next == b']' || next.is_ascii_whitespace() {
                        break;
                    }
                    index += 1;
                }
            }
            _ => return Err(ClaudeHookJsonBound::Invalid),
        }
        index += 1;
    }
    if in_string || depth != 0 {
        return Err(ClaudeHookJsonBound::Invalid);
    }
    Ok(())
}

fn count_hook_json_node(total_nodes: &mut usize) -> Result<(), ClaudeHookJsonBound> {
    *total_nodes = total_nodes.saturating_add(1);
    if *total_nodes > MAX_CLAUDE_HOOK_JSON_TOTAL_NODES {
        return Err(ClaudeHookJsonBound::Invalid);
    }
    Ok(())
}

fn note_array_element(
    array_elements: &mut [usize],
    awaiting_array_value: &mut [bool],
    depth: usize,
    in_array: [bool; MAX_CLAUDE_HOOK_JSON_NESTING + 1],
) -> Result<(), ClaudeHookJsonBound> {
    if depth == 0 || !in_array[depth] || !awaiting_array_value[depth] {
        return Ok(());
    }
    array_elements[depth] = array_elements[depth].saturating_add(1);
    if array_elements[depth] > MAX_CLAUDE_HOOK_JSON_ARRAY_ELEMENTS {
        return Err(ClaudeHookJsonBound::Invalid);
    }
    awaiting_array_value[depth] = false;
    Ok(())
}
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
                if let Some(detail) = value
                    .get("prompt")
                    .and_then(Value::as_str)
                    .and_then(provider_task_notification_detail)
                {
                    return ClaudeReduceOutcome {
                        drafts: vec![self.draft(
                            occurred_at_epoch_ms,
                            SemanticEventKind::Status {
                                state: "subagentCompleted".to_string(),
                                detail: Some(detail),
                            },
                            SemanticRetention::Canonical,
                            deduplication_key,
                        )],
                        degraded: false,
                    };
                }
                self.text_event(
                    occurred_at_epoch_ms,
                    value.get("prompt").and_then(Value::as_str),
                    |text| SemanticEventKind::UserMessage { text },
                    SemanticRetention::Canonical,
                    deduplication_key,
                )
            }
            "MessageDisplay" => self.message_display(&value, occurred_at_epoch_ms),
            "PreToolUse"
                if value.get("tool_name").and_then(Value::as_str) == Some("AskUserQuestion") =>
            {
                self.ask_user_question(&value, occurred_at_epoch_ms, occurrence)
            }
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

    fn ask_user_question(
        &self,
        value: &Value,
        occurred_at_epoch_ms: u64,
        occurrence: u64,
    ) -> ClaudeReduceOutcome {
        let Some(question) = value
            .get("tool_input")
            .and_then(|input| input.get("questions"))
            .and_then(Value::as_array)
            .and_then(|questions| questions.first())
        else {
            return ClaudeReduceOutcome::malformed();
        };
        let Some(prompt) = question.get("question").and_then(Value::as_str) else {
            return ClaudeReduceOutcome::malformed();
        };
        let official_id = official_identifier(value, "tool_use_id");
        let question_id = official_id
            .clone()
            .unwrap_or_else(|| format!("ask-user-question-{occurrence}"));
        let deduplication_key = official_id.as_ref().map(|id| {
            scoped_deduplication_key(
                "claude-ask-user-question",
                &self.provider_session_id(value),
                id,
            )
        });
        let choices = question
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .filter_map(|option| option.get("label").and_then(Value::as_str))
                    .map(bounded_text)
                    .take(16)
                    .collect()
            })
            .unwrap_or_default();
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::Question {
                    question_id,
                    prompt: bounded_text(prompt),
                    choices,
                },
                SemanticRetention::Canonical,
                deduplication_key,
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
        let choices = value
            .get("choices")
            .or_else(|| value.get("options"))
            .and_then(Value::as_array)
            .map(|choices| {
                choices
                    .iter()
                    .filter_map(|choice| {
                        choice
                            .as_str()
                            .or_else(|| choice.get("label").and_then(Value::as_str))
                            .map(bounded_text)
                    })
                    .take(16)
                    .collect()
            })
            .unwrap_or_default();
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::Question {
                    question_id: question_id.clone(),
                    prompt: bounded_text(message),
                    choices,
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
        match official_session_id_str(value) {
            Ok(Some(id)) => id.to_string(),
            Ok(None) | Err(_) => self.fallback_provider_session_id.clone(),
        }
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

fn provider_task_notification_detail(prompt: &str) -> Option<String> {
    let prompt = prompt.trim();
    if !prompt.starts_with("<task-notification>") || !prompt.ends_with("</task-notification>") {
        return None;
    }
    fn element(text: &str, tag: &str) -> Option<String> {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let value = text.split_once(&open)?.1.split_once(&close)?.0.trim();
        (!value.is_empty()).then(|| bounded_text(value))
    }
    let summary = element(prompt, "summary")?;
    let result = element(prompt, "result");
    Some(match result {
        Some(result) => format!("{summary}\n{result}"),
        None => summary,
    })
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OfficialSessionIdError {
    TooLong,
}

fn official_session_id_str(value: &Value) -> Result<Option<&str>, OfficialSessionIdError> {
    let Some(raw) = value.get("session_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.len() > MAX_PROVIDER_SESSION_ID_BYTES {
        return Err(OfficialSessionIdError::TooLong);
    }
    Ok(Some(raw))
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
    pub max_cleanup_paths: usize,
    pub registration_ttl: Duration,
    pub reducer: ClaudeReducerLimits,
}

impl Default for ClaudeRegistryLimits {
    fn default() -> Self {
        Self {
            max_registrations: 128,
            max_body_bytes: MAX_CLAUDE_HOOK_BODY_BYTES,
            max_cleanup_paths: 8,
            registration_ttl: Duration::from_secs(24 * 60 * 60),
            reducer: ClaudeReducerLimits::default(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ClaudeHookRegistration {
    pub nonce: String,
    pub stable_session_key: StableSessionKey,
    pub generation: u64,
}

impl fmt::Debug for ClaudeHookRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeHookRegistration")
            .field("nonce", &"<redacted>")
            .field("stable_session_key", &self.stable_session_key)
            .field("generation", &self.generation)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ClaudeCorrelationBinding {
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    runtime_generation: u64,
    action_epoch: u64,
    process_root: ResourceId,
}

impl ClaudeCorrelationBinding {
    pub(crate) fn new(
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        runtime_generation: u64,
        action_epoch: u64,
        process_root: ResourceId,
    ) -> Self {
        Self {
            task_id,
            agent_session_id,
            runtime_generation,
            action_epoch,
            process_root,
        }
    }

    // Cargo integration tests compile this crate without `cfg(test)`; keep the
    // fixture-only constructor out of release builds while preserving those tests.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn test_new(
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        runtime_generation: u64,
        action_epoch: u64,
        process_root: ResourceId,
    ) -> Self {
        Self::new(
            task_id,
            agent_session_id,
            runtime_generation,
            action_epoch,
            process_root,
        )
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn agent_session_id(&self) -> AgentSessionId {
        self.agent_session_id
    }

    pub fn runtime_generation(&self) -> u64 {
        self.runtime_generation
    }

    pub fn action_epoch(&self) -> u64 {
        self.action_epoch
    }

    pub fn process_root(&self) -> ResourceId {
        self.process_root
    }
}

impl fmt::Debug for ClaudeCorrelationBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeCorrelationBinding")
            .field("runtime_generation", &self.runtime_generation)
            .field("action_epoch", &self.action_epoch)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct ClaudeSealedCorrelation {
    binding: ClaudeCorrelationBinding,
    expected_provider_session_id: Option<ProviderSessionId>,
}

impl fmt::Debug for ClaudeSealedCorrelation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeSealedCorrelation")
            .field("binding", &self.binding)
            .field(
                "expected_provider_session_id",
                &self
                    .expected_provider_session_id
                    .as_ref()
                    .map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ClaudeCorrelatedRegistration {
    nonce: String,
    generation: u64,
    sealed: ClaudeSealedCorrelation,
    journal_key: StableSessionKey,
}

impl fmt::Debug for ClaudeCorrelatedRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeCorrelatedRegistration")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl ClaudeCorrelatedRegistration {
    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    pub fn relay_generation(&self) -> u64 {
        self.generation
    }

    pub fn runtime_generation(&self) -> u64 {
        self.sealed.binding.runtime_generation()
    }

    pub fn binding(&self) -> &ClaudeCorrelationBinding {
        &self.sealed.binding
    }

    pub fn expected_provider_session_id(&self) -> Option<&ProviderSessionId> {
        self.sealed.expected_provider_session_id.as_ref()
    }

    pub fn journal_key(&self) -> &StableSessionKey {
        &self.journal_key
    }

    pub(crate) fn hook_registration(&self) -> ClaudeHookRegistration {
        ClaudeHookRegistration {
            nonce: self.nonce.clone(),
            stable_session_key: self.journal_key.clone(),
            generation: self.generation,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ClaudeAdmittedDelivery {
    registration: ClaudeCorrelatedRegistration,
    provider_session_id: ProviderSessionId,
}

impl ClaudeAdmittedDelivery {
    pub fn provider_session_id(&self) -> &ProviderSessionId {
        &self.provider_session_id
    }

    pub fn nonce(&self) -> &str {
        self.registration.nonce()
    }

    pub fn relay_generation(&self) -> u64 {
        self.registration.relay_generation()
    }

    pub fn binding(&self) -> &ClaudeCorrelationBinding {
        self.registration.binding()
    }

    pub fn registration(&self) -> &ClaudeCorrelatedRegistration {
        &self.registration
    }
}

impl fmt::Debug for ClaudeAdmittedDelivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeAdmittedDelivery")
            .field("relay_generation", &self.registration.relay_generation())
            .field("provider_session_id", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeBindingField {
    Task,
    Agent,
    Generation,
    ActionEpoch,
    ProcessRoot,
    RelayGeneration,
}

#[derive(Clone, PartialEq, Eq)]
pub enum ClaudeCorrelatedIngestError {
    StaleRegistration,
    Rejected,
    Expired,
    BodyTooLarge,
    InvalidPayload,
    ForeignEndpoint,
    BindingMismatch(ClaudeBindingField),
    LatePriorSession,
    ExactResumeMismatch { expected: String, observed: String },
    RebindRejected { bound: String, observed: String },
    NotSessionStart,
    MissingProviderSessionId,
    CorrelationMismatch,
    ProviderSessionIdTooLong,
    SessionIdMismatch,
}

impl fmt::Debug for ClaudeCorrelatedIngestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExactResumeMismatch { .. } => f
                .debug_struct("ExactResumeMismatch")
                .field("expected", &"<redacted>")
                .field("observed", &"<redacted>")
                .finish(),
            Self::RebindRejected { .. } => f
                .debug_struct("RebindRejected")
                .field("bound", &"<redacted>")
                .field("observed", &"<redacted>")
                .finish(),
            Self::StaleRegistration => write!(f, "StaleRegistration"),
            Self::Rejected => write!(f, "Rejected"),
            Self::Expired => write!(f, "Expired"),
            Self::BodyTooLarge => write!(f, "BodyTooLarge"),
            Self::InvalidPayload => write!(f, "InvalidPayload"),
            Self::ForeignEndpoint => write!(f, "ForeignEndpoint"),
            Self::BindingMismatch(field) => f.debug_tuple("BindingMismatch").field(field).finish(),
            Self::LatePriorSession => write!(f, "LatePriorSession"),
            Self::NotSessionStart => write!(f, "NotSessionStart"),
            Self::MissingProviderSessionId => write!(f, "MissingProviderSessionId"),
            Self::CorrelationMismatch => write!(f, "CorrelationMismatch"),
            Self::ProviderSessionIdTooLong => write!(f, "ProviderSessionIdTooLong"),
            Self::SessionIdMismatch => write!(f, "SessionIdMismatch"),
        }
    }
}

struct RegisteredClaudeSession {
    stable_session_key: StableSessionKey,
    generation: u64,
    expires_at: Instant,
    activated: bool,
    reducer: ClaudeReducer,
    ingress_degraded: bool,
    cleanup_paths: Vec<PathBuf>,
    sealed: Option<ClaudeSealedCorrelation>,
    bound_provider_session_id: Option<String>,
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
    insert_observer: RwLock<Option<Arc<dyn Fn(bool) + Send + Sync>>>,
    before_publication_observer: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
}

struct ClaudeGenerationWriteGuards<'a> {
    _publication: std::sync::RwLockWriteGuard<'a, ()>,
    _ingress: std::sync::RwLockWriteGuard<'a, ()>,
}

pub type ClaudeRegistryEventHandler =
    Arc<dyn Fn(ClaudeHookRegistration, ClaudeRegistryEvent) + Send + Sync>;

#[derive(Clone)]
pub enum ClaudeRegistryEvent {
    Semantic(SemanticEventDraft),
    SessionStarted {
        provider_session_id: String,
    },
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

impl fmt::Debug for ClaudeRegistryEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Semantic(draft) => f.debug_tuple("Semantic").field(draft).finish(),
            Self::SessionStarted { .. } => f
                .debug_struct("SessionStarted")
                .field("provider_session_id", &"<redacted>")
                .finish(),
            Self::AdapterHealth {
                stable_session_key,
                health,
            } => f
                .debug_struct("AdapterHealth")
                .field("stable_session_key", stable_session_key)
                .field("health", health)
                .finish(),
            Self::RegistrationDropped {
                stable_session_key,
                generation,
                was_latest,
                ..
            } => f
                .debug_struct("RegistrationDropped")
                .field("stable_session_key", stable_session_key)
                .field("generation", generation)
                .field("was_latest", was_latest)
                .finish_non_exhaustive(),
        }
    }
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
            insert_observer: RwLock::new(None),
            before_publication_observer: RwLock::new(None),
        }
    }

    pub fn set_insert_observer(&self, observer: impl Fn(bool) + Send + Sync + 'static) {
        if let Ok(mut slot) = self.insert_observer.write() {
            *slot = Some(Arc::new(observer));
        }
    }

    pub fn set_before_publication_observer(&self, observer: impl Fn() + Send + Sync + 'static) {
        if let Ok(mut slot) = self.before_publication_observer.write() {
            *slot = Some(Arc::new(observer));
        }
    }

    fn observe_insert(&self, sealed: bool) {
        if let Ok(slot) = self.insert_observer.read() {
            if let Some(observer) = slot.as_ref() {
                observer(sealed);
            }
        }
    }

    fn observe_before_publication(&self) {
        if let Ok(slot) = self.before_publication_observer.read() {
            if let Some(observer) = slot.as_ref() {
                observer();
            }
        }
    }

    fn register_at(
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
                sealed: None,
                bound_provider_session_id: None,
            },
        );
        self.observe_insert(false);
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

    // These raw registry calls are fixture-only for the same integration-test
    // constraint; release callers can reach only the authenticated adapter seam.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn test_register_at(
        &self,
        stable_session_key: StableSessionKey,
        now: Instant,
    ) -> Result<ClaudeHookRegistration, String> {
        self.register_at(stable_session_key, now)
    }

    pub(crate) fn register_correlated_at(
        &self,
        stable_session_key: StableSessionKey,
        binding: ClaudeCorrelationBinding,
        expected_provider_session_id: Option<ProviderSessionId>,
        carry_bound_provider_session_id: Option<String>,
        now: Instant,
    ) -> Result<ClaudeCorrelatedRegistration, String> {
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
        let sealed = ClaudeSealedCorrelation {
            binding,
            expected_provider_session_id,
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
                sealed: Some(sealed.clone()),
                bound_provider_session_id: carry_bound_provider_session_id,
            },
        );
        self.observe_insert(true);
        drop(state);
        drop(publication_guard);
        self.finish_dropped_registrations(removed);
        Ok(ClaudeCorrelatedRegistration {
            nonce,
            generation,
            sealed,
            journal_key: stable_session_key,
        })
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn test_register_correlated_at(
        &self,
        stable_session_key: StableSessionKey,
        binding: ClaudeCorrelationBinding,
        expected_provider_session_id: Option<ProviderSessionId>,
        carry_bound_provider_session_id: Option<String>,
        now: Instant,
    ) -> Result<ClaudeCorrelatedRegistration, String> {
        self.register_correlated_at(
            stable_session_key,
            binding,
            expected_provider_session_id,
            carry_bound_provider_session_id,
            now,
        )
    }

    pub fn bound_provider_session_id(&self, nonce: &str) -> Option<String> {
        self.state.lock().ok().and_then(|state| {
            state
                .registrations
                .get(nonce)?
                .bound_provider_session_id
                .clone()
        })
    }

    pub(crate) fn ingest_correlated_at(
        &self,
        peer: SocketAddr,
        presented: &ClaudeCorrelatedRegistration,
        expected: &ClaudeCorrelationBinding,
        body: &[u8],
        now: Instant,
        occurred_at_epoch_ms: u64,
    ) -> Result<ClaudeAdmittedDelivery, ClaudeCorrelatedIngestError> {
        match physically_bound_claude_hook_json(body) {
            Err(ClaudeHookJsonBound::BodyTooLarge) => {
                return Err(ClaudeCorrelatedIngestError::BodyTooLarge);
            }
            Err(ClaudeHookJsonBound::Invalid) => {
                return Err(ClaudeCorrelatedIngestError::InvalidPayload);
            }
            Ok(()) => {}
        }
        if !peer.ip().is_loopback() {
            return Err(ClaudeCorrelatedIngestError::ForeignEndpoint);
        }
        let context = match self.admit_at(peer, presented.nonce(), body.len(), now) {
            Ok(context) => context,
            Err(RelayIngestStatus::BodyTooLarge) => {
                return Err(ClaudeCorrelatedIngestError::BodyTooLarge);
            }
            Err(RelayIngestStatus::Expired) => {
                return Err(ClaudeCorrelatedIngestError::Expired);
            }
            Err(RelayIngestStatus::Rejected) | Err(RelayIngestStatus::Accepted(_)) => {
                return Err(ClaudeCorrelatedIngestError::Rejected);
            }
        };
        let Ok(mut state) = self.state.lock() else {
            return Err(ClaudeCorrelatedIngestError::Rejected);
        };
        if !context_is_current(&state, &context) {
            return Err(ClaudeCorrelatedIngestError::StaleRegistration);
        }
        let Some(registration) = state.registrations.get_mut(&context.nonce) else {
            return Err(ClaudeCorrelatedIngestError::Rejected);
        };
        let Some(sealed) = registration.sealed.clone() else {
            return Err(ClaudeCorrelatedIngestError::CorrelationMismatch);
        };
        if presented.sealed != sealed || presented.generation != registration.generation {
            return Err(ClaudeCorrelatedIngestError::CorrelationMismatch);
        }
        if presented.generation != registration.generation {
            return Err(ClaudeCorrelatedIngestError::BindingMismatch(
                ClaudeBindingField::RelayGeneration,
            ));
        }
        if let Err(error) = compare_correlation_binding(expected, &sealed.binding) {
            return Err(error);
        }
        let value: Value = serde_json::from_slice(body)
            .map_err(|_| ClaudeCorrelatedIngestError::InvalidPayload)?;
        if value.get("hook_event_name").and_then(Value::as_str) != Some("SessionStart") {
            return Err(ClaudeCorrelatedIngestError::NotSessionStart);
        }
        let raw_session = match official_session_id_str(&value) {
            Err(OfficialSessionIdError::TooLong) => {
                return Err(ClaudeCorrelatedIngestError::ProviderSessionIdTooLong);
            }
            Ok(None) => {
                return Err(ClaudeCorrelatedIngestError::MissingProviderSessionId);
            }
            Ok(Some(raw)) => raw,
        };
        let observed = ProviderSessionId::new(raw_session)
            .map_err(|_| ClaudeCorrelatedIngestError::InvalidPayload)?;
        if let Some(expected_id) = &sealed.expected_provider_session_id {
            if expected_id != &observed {
                return Err(ClaudeCorrelatedIngestError::ExactResumeMismatch {
                    expected: expected_id.as_str().to_string(),
                    observed: observed.as_str().to_string(),
                });
            }
        }
        if let Some(bound) = registration.bound_provider_session_id.clone() {
            if bound != observed.as_str() {
                return Err(ClaudeCorrelatedIngestError::RebindRejected {
                    bound,
                    observed: observed.as_str().to_string(),
                });
            }
        } else {
            registration.bound_provider_session_id = Some(observed.as_str().to_string());
        }
        let delivery = ClaudeAdmittedDelivery {
            registration: presented.clone(),
            provider_session_id: observed.clone(),
        };
        drop(state);
        let mut captured = self.reduce_admitted(context.clone(), body, occurred_at_epoch_ms);
        if matches!(
            &captured.status,
            RelayIngestStatus::Accepted(outcome) if outcome.drafts.is_empty() && !outcome.degraded
        ) && captured.provider_session_id.is_none()
        {
            return Err(ClaudeCorrelatedIngestError::StaleRegistration);
        }
        if let RelayIngestStatus::Accepted(outcome) = &captured.status {
            if outcome.degraded && captured.provider_session_id.is_none() {
                return Err(ClaudeCorrelatedIngestError::InvalidPayload);
            }
        }
        if !self.is_current_registration(&context) {
            return Err(ClaudeCorrelatedIngestError::StaleRegistration);
        }
        captured.provider_session_id = Some(observed.as_str().to_string());
        let status = self.dispatch_captured(captured);
        if !self.is_current_registration(&context) {
            return Err(ClaudeCorrelatedIngestError::StaleRegistration);
        }
        match status {
            RelayIngestStatus::Accepted(_) => Ok(delivery),
            RelayIngestStatus::BodyTooLarge => Err(ClaudeCorrelatedIngestError::BodyTooLarge),
            RelayIngestStatus::Expired => Err(ClaudeCorrelatedIngestError::Expired),
            RelayIngestStatus::Rejected => Err(ClaudeCorrelatedIngestError::Rejected),
        }
    }

    pub(crate) fn validate_hook_session_at(
        &self,
        presented: &ClaudeCorrelatedRegistration,
        expected: &ClaudeCorrelationBinding,
        body: &[u8],
        now: Instant,
    ) -> Result<(), ClaudeCorrelatedIngestError> {
        match physically_bound_claude_hook_json(body) {
            Err(ClaudeHookJsonBound::BodyTooLarge) => {
                return Err(ClaudeCorrelatedIngestError::BodyTooLarge);
            }
            Err(ClaudeHookJsonBound::Invalid) => {
                return Err(ClaudeCorrelatedIngestError::InvalidPayload);
            }
            Ok(()) => {}
        }
        let value: Value = serde_json::from_slice(body)
            .map_err(|_| ClaudeCorrelatedIngestError::InvalidPayload)?;
        let raw_session = match official_session_id_str(&value) {
            Err(OfficialSessionIdError::TooLong) => {
                return Err(ClaudeCorrelatedIngestError::ProviderSessionIdTooLong);
            }
            Ok(None) => return Err(ClaudeCorrelatedIngestError::SessionIdMismatch),
            Ok(Some(raw)) => raw,
        };
        let Ok(state) = self.state.lock() else {
            return Err(ClaudeCorrelatedIngestError::Rejected);
        };
        let Some(registration) = state.registrations.get(presented.nonce()) else {
            return Err(ClaudeCorrelatedIngestError::Rejected);
        };
        if registration.expires_at <= now {
            return Err(ClaudeCorrelatedIngestError::Expired);
        }
        if presented.generation != registration.generation {
            return Err(ClaudeCorrelatedIngestError::CorrelationMismatch);
        }
        let current = ClaudeHookRegistration {
            nonce: presented.nonce().to_string(),
            stable_session_key: registration.stable_session_key.clone(),
            generation: registration.generation,
        };
        if !registration_is_current(&state, &current) {
            return Err(ClaudeCorrelatedIngestError::StaleRegistration);
        }
        let Some(sealed) = registration.sealed.as_ref() else {
            return Err(ClaudeCorrelatedIngestError::CorrelationMismatch);
        };
        if presented.sealed != *sealed {
            return Err(ClaudeCorrelatedIngestError::CorrelationMismatch);
        }
        if let Err(error) = compare_correlation_binding(expected, &sealed.binding) {
            return Err(error);
        }
        let Some(bound) = registration.bound_provider_session_id.as_deref() else {
            return Err(ClaudeCorrelatedIngestError::SessionIdMismatch);
        };
        if bound != raw_session {
            return Err(ClaudeCorrelatedIngestError::SessionIdMismatch);
        }
        Ok(())
    }

    fn ingest_at(
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

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn test_ingest_at(
        &self,
        peer: SocketAddr,
        nonce: &str,
        body: &[u8],
        now: Instant,
        occurred_at_epoch_ms: u64,
    ) -> RelayIngestStatus {
        self.ingest_at(peer, nonce, body, now, occurred_at_epoch_ms)
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
        body: &[u8],
        now: Instant,
        enqueue: impl FnOnce(ClaudeRegistrationContext) -> T,
    ) -> Result<T, RelayIngestStatus> {
        if !peer.ip().is_loopback() {
            return Err(RelayIngestStatus::Rejected);
        }
        match physically_bound_claude_hook_json(body) {
            Ok(()) => {}
            Err(ClaudeHookJsonBound::BodyTooLarge) => {
                return Err(RelayIngestStatus::BodyTooLarge);
            }
            Err(ClaudeHookJsonBound::Invalid) => {
                return Err(RelayIngestStatus::Rejected);
            }
        }
        if body.len() > self.limits.max_body_bytes {
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
            return Err(RelayIngestStatus::Rejected);
        }
        let observed_session_id =
            match reject_uncorrelated_or_mismatched_http_event(registration, body) {
                Ok(observed) => observed,
                Err(status) => return Err(status),
            };
        let registration = state
            .registrations
            .get_mut(nonce)
            .expect("registration checked above");
        if registration.bound_provider_session_id.is_none() {
            if let Some(observed) = observed_session_id {
                registration.bound_provider_session_id = Some(observed);
            }
        }
        if registration.activated {
            registration.expires_at = now + self.limits.registration_ttl;
        }
        drop(state);
        Ok(enqueue(context))
    }

    pub fn admit_http_hook_at(
        &self,
        peer: SocketAddr,
        nonce: &str,
        body: &[u8],
        now: Instant,
    ) -> Result<(), RelayIngestStatus> {
        self.admit_ingress_at(peer, nonce, body, now, |_| ())
    }

    pub fn http_hook_status(result: Result<(), RelayIngestStatus>) -> StatusCode {
        match result {
            Ok(()) => StatusCode::NO_CONTENT,
            Err(RelayIngestStatus::Rejected) | Err(RelayIngestStatus::Accepted(_)) => {
                StatusCode::UNAUTHORIZED
            }
            Err(RelayIngestStatus::BodyTooLarge) => StatusCode::PAYLOAD_TOO_LARGE,
            Err(RelayIngestStatus::Expired) => StatusCode::GONE,
        }
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
                provider_session_id: None,
            };
        }
        if physically_bound_claude_hook_json(body).is_err() {
            return CapturedClaudeIngest {
                status: RelayIngestStatus::Accepted(ClaudeReduceOutcome::malformed()),
                context: Some(context),
                promoted_healthy: false,
                provider_session_id: None,
            };
        }
        let official_session_start_id = match serde_json::from_slice::<Value>(body) {
            Ok(value)
                if value.get("hook_event_name").and_then(Value::as_str) == Some("SessionStart") =>
            {
                match official_session_id_str(&value) {
                    Ok(id) => id.map(str::to_string),
                    Err(OfficialSessionIdError::TooLong) => {
                        return CapturedClaudeIngest {
                            status: RelayIngestStatus::Accepted(ClaudeReduceOutcome::malformed()),
                            context: Some(context),
                            promoted_healthy: false,
                            provider_session_id: None,
                        };
                    }
                }
            }
            _ => None,
        };
        let registration = state
            .registrations
            .get_mut(&context.nonce)
            .expect("current registration exists");
        if let Some(official) = official_session_start_id.as_deref() {
            if let Some(expected) = registration
                .sealed
                .as_ref()
                .and_then(|sealed| sealed.expected_provider_session_id.as_ref())
            {
                if expected.as_str() != official {
                    return CapturedClaudeIngest {
                        status: RelayIngestStatus::Accepted(ClaudeReduceOutcome::ignored()),
                        context: Some(context),
                        promoted_healthy: false,
                        provider_session_id: None,
                    };
                }
            }
            if let Some(bound) = registration.bound_provider_session_id.as_deref() {
                if bound != official {
                    return CapturedClaudeIngest {
                        status: RelayIngestStatus::Accepted(ClaudeReduceOutcome::ignored()),
                        context: Some(context),
                        promoted_healthy: false,
                        provider_session_id: None,
                    };
                }
            } else {
                registration.bound_provider_session_id = Some(official.to_string());
            }
        }
        let is_session_start = official_session_start_id.is_some();
        let outcome = registration.reducer.apply_json(body, occurred_at_epoch_ms);
        let promoted_healthy = is_session_start && !outcome.degraded && !registration.activated;
        let provider_session_id = if outcome.degraded {
            None
        } else {
            official_session_start_id
        };
        if promoted_healthy {
            registration.activated = true;
            registration.expires_at = context.admitted_at + self.limits.registration_ttl;
        }
        CapturedClaudeIngest {
            status: RelayIngestStatus::Accepted(outcome),
            context: Some(context),
            promoted_healthy,
            provider_session_id,
        }
    }

    pub fn set_event_handler(&self, handler: Option<ClaudeRegistryEventHandler>) {
        if let Ok(mut slot) = self.event_handler.write() {
            *slot = handler;
        }
    }

    pub fn attach_cleanup_path(&self, nonce: &str, path: PathBuf) -> bool {
        let evicted = self.state.lock().ok().and_then(|mut state| {
            state.registrations.get_mut(nonce).map(|registration| {
                let evicted =
                    if registration.cleanup_paths.len() >= self.limits.max_cleanup_paths.max(1) {
                        registration.cleanup_paths.first().cloned()
                    } else {
                        None
                    };
                if evicted.is_some() {
                    registration.cleanup_paths.remove(0);
                }
                registration.cleanup_paths.push(path);
                evicted
            })
        });
        let Some(evicted) = evicted else {
            return false;
        };
        if let Some(path) = evicted {
            remove_cleanup_paths(vec![path]);
        }
        true
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
            provider_session_id,
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
        self.observe_before_publication();
        before_publication();
        if !self.is_current_registration(&context) {
            return status;
        }
        if let Some(handler) = handler.as_ref() {
            let registration = context.registration();
            for draft in &outcome.drafts {
                invoke_registry_handler(
                    handler,
                    registration.clone(),
                    ClaudeRegistryEvent::Semantic(draft.clone()),
                );
            }
            if let Some(provider_session_id) = provider_session_id {
                invoke_registry_handler(
                    handler,
                    registration.clone(),
                    ClaudeRegistryEvent::SessionStarted {
                        provider_session_id,
                    },
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
    provider_session_id: Option<String>,
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
            provider_session_id: None,
        }
    }
}

fn reject_uncorrelated_or_mismatched_http_event(
    registration: &RegisteredClaudeSession,
    body: &[u8],
) -> Result<Option<String>, RelayIngestStatus> {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Err(RelayIngestStatus::Rejected);
    };
    let Some(sealed) = registration.sealed.as_ref() else {
        return Err(RelayIngestStatus::Rejected);
    };
    let Some(event_name) = value
        .get("hook_event_name")
        .and_then(Value::as_str)
        .filter(|event| CLAUDE_HOOK_EVENTS.contains(event))
    else {
        return Err(RelayIngestStatus::Rejected);
    };
    let raw = match official_session_id_str(&value) {
        Ok(Some(raw)) => raw,
        Ok(None) | Err(OfficialSessionIdError::TooLong) => {
            return Err(RelayIngestStatus::Rejected);
        }
    };
    if ProviderSessionId::new(raw.to_string()).is_err() {
        return Err(RelayIngestStatus::Rejected);
    }
    if event_name == "SessionStart" {
        if let Some(expected) = sealed.expected_provider_session_id.as_ref() {
            if expected.as_str() != raw {
                return Err(RelayIngestStatus::Rejected);
            }
        }
        if let Some(bound) = registration.bound_provider_session_id.as_deref() {
            if bound != raw {
                return Err(RelayIngestStatus::Rejected);
            }
        }
    } else {
        let Some(bound) = registration.bound_provider_session_id.as_deref() else {
            return Err(RelayIngestStatus::Rejected);
        };
        if bound != raw {
            return Err(RelayIngestStatus::Rejected);
        }
    }
    Ok(Some(raw.to_string()))
}

fn compare_correlation_binding(
    expected: &ClaudeCorrelationBinding,
    sealed: &ClaudeCorrelationBinding,
) -> Result<(), ClaudeCorrelatedIngestError> {
    if expected.task_id != sealed.task_id {
        return Err(ClaudeCorrelatedIngestError::BindingMismatch(
            ClaudeBindingField::Task,
        ));
    }
    if expected.agent_session_id != sealed.agent_session_id {
        return Err(ClaudeCorrelatedIngestError::BindingMismatch(
            ClaudeBindingField::Agent,
        ));
    }
    if expected.runtime_generation < sealed.runtime_generation {
        return Err(ClaudeCorrelatedIngestError::LatePriorSession);
    }
    if expected.runtime_generation != sealed.runtime_generation {
        return Err(ClaudeCorrelatedIngestError::BindingMismatch(
            ClaudeBindingField::Generation,
        ));
    }
    if expected.action_epoch != sealed.action_epoch {
        return Err(ClaudeCorrelatedIngestError::BindingMismatch(
            ClaudeBindingField::ActionEpoch,
        ));
    }
    if expected.process_root != sealed.process_root {
        return Err(ClaudeCorrelatedIngestError::BindingMismatch(
            ClaudeBindingField::ProcessRoot,
        ));
    }
    Ok(())
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
    pub max_connections: usize,
    pub max_in_flight: usize,
}

const MAX_CLAUDE_INGRESS_CRITICAL_EVENTS: usize = 4 * 1024;
const MAX_CLAUDE_INGRESS_OPTIONAL_EVENTS: usize = 1024;
const MAX_CLAUDE_INGRESS_CRITICAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_CLAUDE_INGRESS_OPTIONAL_BYTES: usize = 4 * 1024 * 1024;
const MAX_CLAUDE_INGRESS_CONNECTIONS: usize = 1024;
const MAX_CLAUDE_INGRESS_IN_FLIGHT: usize = 1024;

impl ClaudeIngressLimits {
    pub fn new(
        max_critical_events: usize,
        max_optional_events: usize,
        max_critical_bytes: usize,
        max_optional_bytes: usize,
        max_connections: usize,
        max_in_flight: usize,
    ) -> Self {
        Self {
            max_critical_events,
            max_optional_events,
            max_critical_bytes,
            max_optional_bytes,
            max_connections,
            max_in_flight,
        }
        .bounded()
    }

    fn bounded(self) -> Self {
        Self {
            max_critical_events: self
                .max_critical_events
                .clamp(1, MAX_CLAUDE_INGRESS_CRITICAL_EVENTS),
            max_optional_events: self
                .max_optional_events
                .clamp(1, MAX_CLAUDE_INGRESS_OPTIONAL_EVENTS),
            max_critical_bytes: self
                .max_critical_bytes
                .clamp(1, MAX_CLAUDE_INGRESS_CRITICAL_BYTES),
            max_optional_bytes: self
                .max_optional_bytes
                .clamp(1, MAX_CLAUDE_INGRESS_OPTIONAL_BYTES),
            max_connections: self
                .max_connections
                .clamp(1, MAX_CLAUDE_INGRESS_CONNECTIONS),
            max_in_flight: self.max_in_flight.clamp(1, MAX_CLAUDE_INGRESS_IN_FLIGHT),
        }
    }
}

impl Default for ClaudeIngressLimits {
    fn default() -> Self {
        Self::new(256, 64, 4 * 1024 * 1024, 1024 * 1024, 64, 64)
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
    connection_slots: Arc<Semaphore>,
    in_flight_slots: Arc<Semaphore>,
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
        let limits = limits.bounded();
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
            connection_slots: Arc::new(Semaphore::new(limits.max_connections.max(1))),
            in_flight_slots: Arc::new(Semaphore::new(limits.max_in_flight.max(1))),
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
                        .layer(middleware::from_fn_with_state(
                            ingress_state.clone(),
                            limit_claude_connections,
                        ))
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
    let Ok(_in_flight_permit) = ingress.in_flight_slots.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    let Some(nonce) = headers
        .get("x-devmanager-claude-nonce")
        .and_then(|value| value.to_str().ok())
    else {
        return StatusCode::UNAUTHORIZED;
    };
    if let Err(bound) = physically_bound_claude_hook_json(&body) {
        return match bound {
            ClaudeHookJsonBound::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ClaudeHookJsonBound::Invalid => StatusCode::BAD_REQUEST,
        };
    }
    let optional = is_optional_claude_hook(&body);
    let admission_registry = ingress.registry.clone();
    let queue_registry = ingress.registry.clone();
    let queue = ingress.queue.clone();
    let limits = ingress.limits;
    ClaudeHookRegistry::http_hook_status(admission_registry.admit_ingress_at(
        peer,
        nonce,
        &body,
        Instant::now(),
        |context| {
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
    ))
}

async fn limit_claude_connections(
    State(ingress): State<ClaudeIngressState>,
    request: Request,
    next: Next,
) -> Response {
    let Ok(_connection_permit) = ingress.connection_slots.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    next.run(request).await
}

fn is_optional_claude_hook(body: &[u8]) -> bool {
    if physically_bound_claude_hook_json(body).is_err() {
        return false;
    }
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
    is_valid_loopback_relay_url_for(endpoint, "/internal/claude-hook")
}

pub fn is_valid_loopback_relay_url_for(endpoint: &str, expected_path: &str) -> bool {
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
    path_and_query.path() == expected_path && path_and_query.query().is_none()
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

#[derive(Clone)]
pub struct ClaudeLaunchOverlay {
    pub startup_command: String,
    pub endpoint: String,
    pub registration: Option<ClaudeHookRegistration>,
    pub settings_path: Option<PathBuf>,
    pub health: SemanticAdapterHealth,
    pub diagnostic: Option<String>,
}

impl fmt::Debug for ClaudeLaunchOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeLaunchOverlay")
            .field("has_startup_command", &(!self.startup_command.is_empty()))
            .field("has_endpoint", &(!self.endpoint.is_empty()))
            .field("has_registration", &self.registration.is_some())
            .field("has_settings_path", &self.settings_path.is_some())
            .field("health", &self.health)
            .field("has_diagnostic", &self.diagnostic.is_some())
            .finish()
    }
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
    prepare_claude_launch_overlay_with_registration(
        registry,
        stable_session_key,
        startup_command,
        shell,
        devmanager_executable,
        endpoint,
        temp_root,
        now,
        |registry, stable_session_key, now| registry.register_at(stable_session_key, now),
    )
}

/// Production launch preparation: the relay registration is sealed to the
/// exact launch correlation before its nonce is written into the settings
/// overlay. HTTP ingress therefore never observes an uncorrelated production
/// registration, even during launch or cleanup races.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_correlated_claude_launch_overlay(
    registry: &ClaudeHookRegistry,
    stable_session_key: StableSessionKey,
    binding: ClaudeCorrelationBinding,
    expected_provider_session_id: Option<ProviderSessionId>,
    startup_command: &str,
    shell: ClaudeShellKind,
    devmanager_executable: &Path,
    endpoint: &str,
    temp_root: &Path,
    now: Instant,
) -> ClaudeLaunchOverlay {
    prepare_claude_launch_overlay_with_registration(
        registry,
        stable_session_key,
        startup_command,
        shell,
        devmanager_executable,
        endpoint,
        temp_root,
        now,
        move |registry, stable_session_key, now| {
            registry
                .register_correlated_at(
                    stable_session_key,
                    binding,
                    expected_provider_session_id,
                    None,
                    now,
                )
                .map(|registration| registration.hook_registration())
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_claude_launch_overlay_with_registration<F>(
    registry: &ClaudeHookRegistry,
    stable_session_key: StableSessionKey,
    startup_command: &str,
    shell: ClaudeShellKind,
    devmanager_executable: &Path,
    endpoint: &str,
    temp_root: &Path,
    now: Instant,
    register: F,
) -> ClaudeLaunchOverlay
where
    F: FnOnce(
        &ClaudeHookRegistry,
        StableSessionKey,
        Instant,
    ) -> Result<ClaudeHookRegistration, String>,
{
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

    let registration = match register(registry, stable_session_key, now) {
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
        let mut relay = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": devmanager_executable.display().to_string(),
                "args": ["claude-hook-relay", "--url", endpoint, "--nonce", nonce]
            }]
        });
        // PreToolUse must reach the host before Claude paints and blocks on an
        // interactive AskUserQuestion prompt. Other observational hooks remain
        // asynchronous so ordinary tool and output progress never stalls.
        if *event != "PreToolUse" {
            relay["hooks"][0]["async"] = Value::Bool(true);
        }
        event_hooks.push(relay);
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

pub(crate) fn is_safe_cmd_settings_root(path: &Path) -> bool {
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
    fn session_start_publishes_official_provider_session_id_for_current_generation_only() {
        let registry = ClaudeHookRegistry::default();
        let registration = registry
            .register_at(StableSessionKey::from_tab("claude-tab"), Instant::now())
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        registry.set_event_handler(Some(Arc::new(move |_registration, event| {
            observed.lock().unwrap().push(event);
        })));

        let captured = registry.ingest_captured_at(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45000),
            &registration.nonce,
            br#"{"hook_event_name":"SessionStart","session_id":"provider-123","source":"startup"}"#,
            Instant::now(),
            1_800_000_000_000,
        );
        let status = registry.dispatch_captured(captured);
        assert!(matches!(status, RelayIngestStatus::Accepted(_)));
        let published: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                ClaudeRegistryEvent::SessionStarted {
                    provider_session_id,
                } => Some(provider_session_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(published, vec!["provider-123".to_string()]);

        events.lock().unwrap().clear();
        let captured = registry.ingest_captured_at(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45001),
            &registration.nonce,
            br#"{"hook_event_name":"SessionStart","session_id":"stale-provider","source":"startup"}"#,
            Instant::now(),
            1_800_000_000_001,
        );
        let _replacement = registry
            .register_at(StableSessionKey::from_tab("claude-tab"), Instant::now())
            .unwrap();
        let _ = registry.dispatch_captured(captured);
        assert!(!events
            .lock()
            .unwrap()
            .iter()
            .any(|event| { matches!(event, ClaudeRegistryEvent::SessionStarted { .. }) }));
    }

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

    #[test]
    fn array_heavy_hook_json_fails_physical_bounds_before_serde() {
        let mut body =
            br#"{"hook_event_name":"SessionStart","session_id":"session-1","pad":["#.to_vec();
        for index in 0..40 {
            if index > 0 {
                body.push(b',');
            }
            body.push(b'0');
        }
        body.extend_from_slice(b"]}");
        assert_eq!(
            physically_bound_claude_hook_json(&body),
            Err(ClaudeHookJsonBound::Invalid)
        );
    }

    fn race_binding() -> ClaudeCorrelationBinding {
        ClaudeCorrelationBinding::new(
            TaskId::new(),
            AgentSessionId::new(),
            1,
            1,
            ResourceId::new(),
        )
    }

    #[test]
    fn register_correlated_insert_is_already_sealed() {
        let registry = ClaudeHookRegistry::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        registry.set_insert_observer(move |sealed| {
            observed.lock().unwrap().push(sealed);
        });
        let expected = ProviderSessionId::new("session-1").unwrap();
        let registered = registry
            .register_correlated_at(
                StableSessionKey::from_tab("journal-tab"),
                race_binding(),
                Some(expected.clone()),
                None,
                Instant::now(),
            )
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![true]);
        assert_eq!(
            registered
                .expected_provider_session_id()
                .map(ProviderSessionId::as_str),
            Some("session-1")
        );
        assert!(registered.binding().runtime_generation() == 1);
        assert_eq!(registry.bound_provider_session_id(registered.nonce()), None);
    }

    #[test]
    fn correlated_replacement_during_dispatch_is_stale_not_accepted_success() {
        let registry = Arc::new(ClaudeHookRegistry::default());
        let binding = race_binding();
        let journal = StableSessionKey::from_tab("journal-tab");
        let first = registry
            .register_correlated_at(journal.clone(), binding.clone(), None, None, Instant::now())
            .unwrap();
        let events = Arc::new(Mutex::new(0_usize));
        let observed = events.clone();
        registry.set_event_handler(Some(Arc::new(move |_, event| {
            if matches!(
                event,
                ClaudeRegistryEvent::SessionStarted { .. } | ClaudeRegistryEvent::Semantic(_)
            ) {
                *observed.lock().unwrap() += 1;
            }
        })));

        let captured = registry.ingest_captured_at(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45000),
            first.nonce(),
            br#"{"hook_event_name":"SessionStart","session_id":"session-1","source":"startup"}"#,
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
            assert!(!timeout.timed_out(), "dispatch never paused");
            drop(state);
        }
        let _replacement = registry
            .register_correlated_at(
                journal,
                ClaudeCorrelationBinding::new(
                    binding.task_id(),
                    binding.agent_session_id(),
                    binding.runtime_generation() + 1,
                    binding.action_epoch(),
                    binding.process_root(),
                ),
                None,
                None,
                Instant::now(),
            )
            .unwrap();
        {
            let (lock, condition) = &*gate;
            let mut state = lock.lock().unwrap();
            state.1 = true;
            condition.notify_all();
        }
        let _ = dispatch.join().unwrap();
        assert_eq!(*events.lock().unwrap(), 0);

        let error = registry
            .ingest_correlated_at(
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45000),
                &first,
                &binding,
                br#"{"hook_event_name":"SessionStart","session_id":"session-1","source":"startup"}"#,
                Instant::now(),
                1_800_000_000_000,
            )
            .expect_err("replaced generation must not map Accepted(ignored) to success");
        assert_eq!(error, ClaudeCorrelatedIngestError::StaleRegistration);
    }

    #[test]
    fn correlated_queue_keeps_one_current_generation_per_journal_key() {
        let registry = ClaudeHookRegistry::default();
        let binding = race_binding();
        let journal = StableSessionKey::from_tab("journal-tab");
        let first = registry
            .register_correlated_at(journal.clone(), binding.clone(), None, None, Instant::now())
            .unwrap();
        let second = registry
            .register_correlated_at(
                journal,
                ClaudeCorrelationBinding::new(
                    binding.task_id(),
                    binding.agent_session_id(),
                    binding.runtime_generation() + 1,
                    binding.action_epoch(),
                    binding.process_root(),
                ),
                None,
                None,
                Instant::now(),
            )
            .unwrap();
        assert!(second.relay_generation() > first.relay_generation());
        let error = registry
            .ingest_correlated_at(
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45000),
                &first,
                &binding,
                br#"{"hook_event_name":"SessionStart","session_id":"session-1","source":"startup"}"#,
                Instant::now(),
                1_800_000_000_000,
            )
            .expect_err("only the latest sealed generation is current");
        assert_eq!(error, ClaudeCorrelatedIngestError::StaleRegistration);
    }

    #[test]
    fn drop_unregisters_before_dispatch_cannot_publish_or_succeed() {
        let registry = ClaudeHookRegistry::default();
        let binding = race_binding();
        let registered = registry
            .register_correlated_at(
                StableSessionKey::from_tab("journal-tab"),
                binding.clone(),
                None,
                None,
                Instant::now(),
            )
            .unwrap();
        let events = Arc::new(Mutex::new(0_usize));
        let observed = events.clone();
        registry.set_event_handler(Some(Arc::new(move |_, event| {
            if matches!(event, ClaudeRegistryEvent::SessionStarted { .. }) {
                *observed.lock().unwrap() += 1;
            }
        })));
        let captured = registry.ingest_captured_at(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45000),
            registered.nonce(),
            br#"{"hook_event_name":"SessionStart","session_id":"session-1","source":"startup"}"#,
            Instant::now(),
            1_800_000_000_000,
        );
        registry.unregister(registered.nonce());
        let _ = registry.dispatch_captured(captured);
        assert_eq!(*events.lock().unwrap(), 0);
        let error = registry
            .ingest_correlated_at(
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45000),
                &registered,
                &binding,
                br#"{"hook_event_name":"SessionStart","session_id":"session-1","source":"startup"}"#,
                Instant::now(),
                1_800_000_000_000,
            )
            .expect_err("dropped registration cannot succeed after unregister");
        assert!(matches!(
            error,
            ClaudeCorrelatedIngestError::Rejected | ClaudeCorrelatedIngestError::StaleRegistration
        ));
    }

    #[test]
    fn http_session_start_admission_rejects_stale_uncorrelated_and_rebind() {
        let registry = ClaudeHookRegistry::default();
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45000);
        let session_start =
            br#"{"hook_event_name":"SessionStart","session_id":"session-1","source":"startup"}"#;
        let rebound =
            br#"{"hook_event_name":"SessionStart","session_id":"session-other","source":"startup"}"#;
        let resume_fresh =
            br#"{"hook_event_name":"SessionStart","session_id":"session-fresh","source":"startup"}"#;

        let unsealed = registry
            .register_at(StableSessionKey::from_tab("journal-tab"), Instant::now())
            .unwrap();
        let uncorrelated = registry
            .admit_ingress_at(peer, &unsealed.nonce, session_start, Instant::now(), |_| ())
            .expect_err("uncorrelated SessionStart must not be HTTP-admitted");
        assert!(matches!(uncorrelated, RelayIngestStatus::Rejected));
        assert!(!matches!(uncorrelated, RelayIngestStatus::Accepted(_)));

        let binding = race_binding();
        let expected = ProviderSessionId::new("session-1").unwrap();
        let sealed = registry
            .register_correlated_at(
                StableSessionKey::from_tab("journal-tab"),
                binding.clone(),
                Some(expected),
                None,
                Instant::now(),
            )
            .unwrap();
        registry
            .admit_ingress_at(peer, sealed.nonce(), session_start, Instant::now(), |_| ())
            .expect("current sealed exact-resume SessionStart is admitted");

        let resume_mismatch = registry
            .admit_ingress_at(peer, sealed.nonce(), resume_fresh, Instant::now(), |_| ())
            .expect_err("exact resume must not fall back fresh on HTTP admit");
        assert!(matches!(resume_mismatch, RelayIngestStatus::Rejected));
        assert!(!matches!(resume_mismatch, RelayIngestStatus::Accepted(_)));

        registry
            .ingest_correlated_at(
                peer,
                &sealed,
                &binding,
                session_start,
                Instant::now(),
                1_800_000_000_000,
            )
            .expect("first valid id binds");
        let rebind = registry
            .admit_ingress_at(peer, sealed.nonce(), rebound, Instant::now(), |_| ())
            .expect_err("different-id rebind must not be HTTP-admitted");
        assert!(matches!(rebind, RelayIngestStatus::Rejected));
        assert!(!matches!(rebind, RelayIngestStatus::Accepted(_)));

        let _replacement = registry
            .register_correlated_at(
                StableSessionKey::from_tab("journal-tab"),
                ClaudeCorrelationBinding::new(
                    binding.task_id(),
                    binding.agent_session_id(),
                    binding.runtime_generation() + 1,
                    binding.action_epoch(),
                    binding.process_root(),
                ),
                None,
                None,
                Instant::now(),
            )
            .unwrap();
        let stale = registry
            .admit_ingress_at(peer, sealed.nonce(), session_start, Instant::now(), |_| ())
            .expect_err("stale SessionStart must not be HTTP-admitted");
        assert!(matches!(stale, RelayIngestStatus::Rejected));
        assert!(!matches!(stale, RelayIngestStatus::Accepted(_)));
    }

    #[test]
    fn correlation_debug_redacts_provider_session_ids() {
        let sealed = registry_debug_registration();
        let rendered = format!("{sealed:?}");
        assert!(!rendered.contains("session-secret"));
        let event = ClaudeRegistryEvent::SessionStarted {
            provider_session_id: "session-secret".to_string(),
        };
        assert!(!format!("{event:?}").contains("session-secret"));
        assert!(format!("{event:?}").contains("<redacted>"));
    }

    fn registry_debug_registration() -> ClaudeCorrelatedRegistration {
        ClaudeHookRegistry::default()
            .register_correlated_at(
                StableSessionKey::from_tab("journal-tab"),
                race_binding(),
                Some(ProviderSessionId::new("session-secret").unwrap()),
                None,
                Instant::now(),
            )
            .unwrap()
    }
}

#[cfg(test)]
mod ai_acceptance_tests {
    use super::*;

    #[test]
    fn ai_acceptance_provider_task_notification_is_subagent_status_not_user_message() {
        let mut reducer = ClaudeReducer::new(
            StableSessionKey::from_tab("subagent-tab"),
            ClaudeReducerLimits::default(),
        );
        let body = br#"{
            "hook_event_name":"UserPromptSubmit",
            "prompt":"<task-notification><summary>Agent \"Inspect AGENTS.md\" finished</summary><result># DevManager Agent Guidance</result></task-notification>"
        }"#;

        let outcome = reducer.apply_json(body, 42);
        assert!(matches!(
            outcome.drafts.as_slice(),
            [SemanticEventDraft {
                kind: SemanticEventKind::Status { state, detail: Some(detail) },
                ..
            }] if state == "subagentCompleted"
                && detail.contains("Inspect AGENTS.md")
                && detail.contains("DevManager Agent Guidance")
        ));
    }

    #[test]
    fn ai_acceptance_elicitation_preserves_provider_question_choices() {
        let mut reducer = ClaudeReducer::new(
            StableSessionKey::from_tab("question-tab"),
            ClaudeReducerLimits::default(),
        );
        let body = br#"{
            "hook_event_name":"Elicitation",
            "elicitation_id":"question-1",
            "message":"Choose the acceptance color",
            "options":[{"label":"Green"},{"label":"Blue"}]
        }"#;

        let outcome = reducer.apply_json(body, 43);
        assert!(matches!(
            outcome.drafts.as_slice(),
            [SemanticEventDraft {
                kind: SemanticEventKind::Question { prompt, choices, .. },
                ..
            }] if prompt == "Choose the acceptance color"
                && choices == &["Green".to_string(), "Blue".to_string()]
        ));
    }

    #[test]
    fn ai_acceptance_ask_user_question_preserves_prompt_and_choices() {
        let mut reducer = ClaudeReducer::new(
            StableSessionKey::from_tab("question-tab"),
            ClaudeReducerLimits::default(),
        );
        let body = br#"{
            "hook_event_name":"PreToolUse",
            "session_id":"claude-session",
            "tool_name":"AskUserQuestion",
            "tool_use_id":"toolu-question-1",
            "tool_input":{"questions":[{
                "question":"Pick a color",
                "header":"Color",
                "options":[{"label":"Blue"},{"label":"Green"}],
                "multiSelect":false
            }]}
        }"#;

        let outcome = reducer.apply_json(body, 44);
        assert!(matches!(
            outcome.drafts.as_slice(),
            [SemanticEventDraft {
                kind: SemanticEventKind::Question { question_id, prompt, choices },
                ..
            }] if question_id == "toolu-question-1"
                && prompt == "Pick a color"
                && choices == &["Blue".to_string(), "Green".to_string()]
        ));
    }

    #[test]
    fn ai_acceptance_pre_tool_use_relay_is_synchronous() {
        let mut settings = serde_json::json!({});
        merge_relay_hooks(
            &mut settings,
            std::path::Path::new("devmanager-host"),
            "http://127.0.0.1:1234/claude-hook",
            "aabbccdd",
        )
        .expect("merge hooks");

        assert_eq!(
            settings["hooks"]["PreToolUse"][0]["hooks"][0].get("async"),
            None
        );
        assert_eq!(
            settings["hooks"]["PostToolUse"][0]["hooks"][0]["async"],
            Value::Bool(true)
        );
    }
}
