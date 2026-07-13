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
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MAX_CLAUDE_HOOK_BODY_BYTES: usize = 256 * 1024;
const MAX_PROVIDER_TEXT_BYTES: usize = 48 * 1024;
const MAX_CLAUDE_SETTINGS_BYTES: usize = 1024 * 1024;
const CLAUDE_NONCE_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeReducerLimits {
    pub max_tool_records: usize,
    pub max_deduplication_keys: usize,
}

impl Default for ClaudeReducerLimits {
    fn default() -> Self {
        Self {
            max_tool_records: 512,
            max_deduplication_keys: 2_048,
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
    limits: ClaudeReducerLimits,
    tools: HashMap<String, ToolRecord>,
    tool_clock: u64,
    deduplication_keys: HashSet<u64>,
    deduplication_order: VecDeque<u64>,
}

impl ClaudeReducer {
    pub fn new(stable_session_key: StableSessionKey, limits: ClaudeReducerLimits) -> Self {
        Self {
            stable_session_key,
            limits,
            tools: HashMap::new(),
            tool_clock: 0,
            deduplication_keys: HashSet::new(),
            deduplication_order: VecDeque::new(),
        }
    }

    pub fn tool(&self, tool_use_id: &str) -> Option<ClaudeToolSnapshot> {
        self.tools
            .get(tool_use_id)
            .map(|record| record.snapshot.clone())
    }

    pub fn tool_record_count(&self) -> usize {
        self.tools.len()
    }

    pub fn deduplication_key_count(&self) -> usize {
        self.deduplication_keys.len()
    }

    pub fn apply_json(&mut self, body: &[u8], occurred_at_epoch_ms: u64) -> ClaudeReduceOutcome {
        let value: Value = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => return ClaudeReduceOutcome::malformed(),
        };
        let Some(event_name) = value.get("hook_event_name").and_then(Value::as_str) else {
            return ClaudeReduceOutcome::malformed();
        };

        let fingerprint = body_fingerprint(body);
        if self.deduplication_keys.contains(&fingerprint) {
            return ClaudeReduceOutcome::ignored();
        }

        let outcome = match event_name {
            "SessionStart" => self.status(
                occurred_at_epoch_ms,
                "started",
                value.get("source").and_then(Value::as_str),
                Some(format!("claude-session-start:{fingerprint}")),
            ),
            "UserPromptSubmit" => self.text_event(
                occurred_at_epoch_ms,
                value.get("prompt").and_then(Value::as_str),
                |text| SemanticEventKind::UserMessage { text },
                SemanticRetention::Canonical,
                None,
            ),
            "MessageDisplay" => {
                let text = value
                    .get("message")
                    .or_else(|| value.get("text"))
                    .or_else(|| value.get("content"))
                    .and_then(Value::as_str);
                self.text_event(
                    occurred_at_epoch_ms,
                    text,
                    |text| SemanticEventKind::AssistantMessage {
                        message_id: public_id(&value, "message_id", "message", fingerprint),
                        text,
                        streaming: true,
                    },
                    SemanticRetention::Verbose,
                    value
                        .get("message_id")
                        .and_then(Value::as_str)
                        .map(|id| format!("claude-message:{id}")),
                )
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
                self.permission_question(&value, occurred_at_epoch_ms, fingerprint)
            }
            "PermissionDenied" => self.permission_denied(&value, occurred_at_epoch_ms),
            "Notification" => self.notification(&value, occurred_at_epoch_ms, fingerprint),
            "Elicitation" => self.elicitation(&value, occurred_at_epoch_ms, fingerprint),
            "ElicitationResult" => self.status(
                occurred_at_epoch_ms,
                "questionAnswered",
                value.get("action").and_then(Value::as_str),
                Some(format!("claude-elicitation-result:{fingerprint}")),
            ),
            "Stop" => self.stop(&value, occurred_at_epoch_ms, fingerprint),
            "StopFailure" => self.stop_failure(&value, occurred_at_epoch_ms),
            "SessionEnd" => self.status(
                occurred_at_epoch_ms,
                "ended",
                value.get("reason").and_then(Value::as_str),
                Some(format!("claude-session-end:{fingerprint}")),
            ),
            "PostToolBatch" => self.status(
                occurred_at_epoch_ms,
                "toolsCompleted",
                None,
                Some(format!("claude-tool-batch:{fingerprint}")),
            ),
            "SubagentStart" | "SubagentStop" | "TaskCreated" | "TaskCompleted" | "PreCompact"
            | "PostCompact" => {
                self.lifecycle_status(event_name, &value, occurred_at_epoch_ms, fingerprint)
            }
            _ => ClaudeReduceOutcome::ignored(),
        };

        if !outcome.degraded {
            self.remember_fingerprint(fingerprint);
        }
        outcome
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
        let tool_use_id = bounded_identifier(tool_use_id);
        let name = bounded_identifier(name);
        if tool_use_id.is_empty() || name.is_empty() {
            return ClaudeReduceOutcome::malformed();
        }

        self.tool_clock = self.tool_clock.wrapping_add(1);
        let mut changed = false;
        let record = self.tools.entry(tool_use_id.clone()).or_insert_with(|| {
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
                Some(format!("claude-tool:{tool_use_id}")),
            )],
            degraded: false,
        }
    }

    fn permission_question(
        &self,
        value: &Value,
        occurred_at_epoch_ms: u64,
        fingerprint: u64,
    ) -> ClaudeReduceOutcome {
        let Some(tool_name) = value.get("tool_name").and_then(Value::as_str) else {
            return ClaudeReduceOutcome::malformed();
        };
        let question_id = public_id(value, "request_id", "permission", fingerprint);
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
                Some(format!("claude-question:{question_id}")),
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

    fn notification(
        &self,
        value: &Value,
        occurred_at_epoch_ms: u64,
        fingerprint: u64,
    ) -> ClaudeReduceOutcome {
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
                Some(format!("claude-notification:{fingerprint}")),
            )],
            degraded: false,
        }
    }

    fn elicitation(
        &self,
        value: &Value,
        occurred_at_epoch_ms: u64,
        fingerprint: u64,
    ) -> ClaudeReduceOutcome {
        let Some(message) = value.get("message").and_then(Value::as_str) else {
            return ClaudeReduceOutcome::malformed();
        };
        let question_id = public_id(value, "elicitation_id", "elicitation", fingerprint);
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::Question {
                    question_id: question_id.clone(),
                    prompt: bounded_text(message),
                    choices: Vec::new(),
                },
                SemanticRetention::Canonical,
                Some(format!("claude-question:{question_id}")),
            )],
            degraded: false,
        }
    }

    fn stop(
        &self,
        value: &Value,
        occurred_at_epoch_ms: u64,
        fingerprint: u64,
    ) -> ClaudeReduceOutcome {
        let mut drafts = Vec::new();
        if let Some(message) = value
            .get("last_assistant_message")
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
        {
            drafts.push(self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::AssistantMessage {
                    message_id: format!("stop-{fingerprint:x}"),
                    text: bounded_text(message),
                    streaming: false,
                },
                SemanticRetention::Canonical,
                Some(format!("claude-stop-message:{fingerprint}")),
            ));
        }
        drafts.push(self.draft(
            occurred_at_epoch_ms,
            SemanticEventKind::Status {
                state: "ready".to_string(),
                detail: None,
            },
            SemanticRetention::Canonical,
            Some(format!("claude-stop:{fingerprint}")),
        ));
        ClaudeReduceOutcome {
            drafts,
            degraded: false,
        }
    }

    fn stop_failure(&self, value: &Value, occurred_at_epoch_ms: u64) -> ClaudeReduceOutcome {
        let Some(error) = value.get("error").and_then(Value::as_str) else {
            return ClaudeReduceOutcome::malformed();
        };
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::Error {
                    message: format!("Claude turn failed: {}", bounded_identifier(error)),
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
        fingerprint: u64,
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
        ClaudeReduceOutcome {
            drafts: vec![self.draft(
                occurred_at_epoch_ms,
                SemanticEventKind::Status {
                    state: state.to_string(),
                    detail,
                },
                SemanticRetention::Canonical,
                Some(format!("claude-lifecycle:{fingerprint}")),
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

    fn remember_fingerprint(&mut self, fingerprint: u64) {
        let limit = self.limits.max_deduplication_keys;
        if limit == 0 {
            return;
        }
        if self.deduplication_keys.insert(fingerprint) {
            self.deduplication_order.push_back(fingerprint);
        }
        while self.deduplication_order.len() > limit {
            if let Some(oldest) = self.deduplication_order.pop_front() {
                self.deduplication_keys.remove(&oldest);
            }
        }
    }
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

fn body_fingerprint(body: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    hasher.finish()
}

fn public_id(value: &Value, field: &str, prefix: &str, fingerprint: u64) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(bounded_identifier)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("{prefix}-{fingerprint:x}"))
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
}

struct RegisteredClaudeSession {
    stable_session_key: StableSessionKey,
    expires_at: Instant,
    reducer: ClaudeReducer,
    cleanup_paths: Vec<PathBuf>,
}

struct ClaudeRegistryState {
    registrations: HashMap<String, RegisteredClaudeSession>,
    order: VecDeque<String>,
}

pub struct ClaudeHookRegistry {
    limits: ClaudeRegistryLimits,
    state: Mutex<ClaudeRegistryState>,
    event_handler: RwLock<Option<ClaudeRegistryEventHandler>>,
}

pub type ClaudeRegistryEventHandler = Arc<dyn Fn(ClaudeRegistryEvent) + Send + Sync>;

#[derive(Debug, Clone)]
pub enum ClaudeRegistryEvent {
    Semantic(SemanticEventDraft),
    AdapterHealth {
        stable_session_key: StableSessionKey,
        health: SemanticAdapterHealth,
    },
    SessionEnded {
        stable_session_key: StableSessionKey,
        nonce: String,
    },
    RegistrationDropped {
        stable_session_key: StableSessionKey,
        nonce: String,
    },
}

struct RemovedClaudeRegistration {
    nonce: String,
    stable_session_key: StableSessionKey,
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
            state: Mutex::new(ClaudeRegistryState {
                registrations: HashMap::new(),
                order: VecDeque::new(),
            }),
            event_handler: RwLock::new(None),
        }
    }

    pub fn register_at(
        &self,
        stable_session_key: StableSessionKey,
        now: Instant,
    ) -> Result<ClaudeHookRegistration, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Claude hook registry lock is poisoned".to_string())?;
        let mut removed = remove_expired(&mut state, now);
        while state.registrations.len() >= self.limits.max_registrations.max(1) {
            let Some(oldest) = state.order.pop_front() else {
                break;
            };
            if let Some(registration) = state.registrations.remove(&oldest) {
                removed.push(removed_registration(oldest, registration));
            }
        }

        let nonce = loop {
            let candidate = match random_nonce() {
                Ok(candidate) => candidate,
                Err(error) => {
                    drop(state);
                    self.finish_dropped_registrations(removed);
                    return Err(error);
                }
            };
            if !state.registrations.contains_key(&candidate) {
                break candidate;
            }
        };
        state.order.push_back(nonce.clone());
        state.registrations.insert(
            nonce.clone(),
            RegisteredClaudeSession {
                stable_session_key: stable_session_key.clone(),
                expires_at: now + self.limits.registration_ttl,
                reducer: ClaudeReducer::new(stable_session_key.clone(), self.limits.reducer),
                cleanup_paths: Vec::new(),
            },
        );
        let registration = ClaudeHookRegistration {
            nonce,
            stable_session_key,
        };
        drop(state);
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
        if !peer.ip().is_loopback() {
            return CapturedClaudeIngest::without_session(RelayIngestStatus::Rejected);
        }
        if body.len() > self.limits.max_body_bytes {
            return CapturedClaudeIngest::without_session(RelayIngestStatus::BodyTooLarge);
        }
        let Ok(mut state) = self.state.lock() else {
            return CapturedClaudeIngest::without_session(RelayIngestStatus::Rejected);
        };
        if state
            .registrations
            .get(nonce)
            .is_some_and(|registration| now > registration.expires_at)
        {
            state.order.retain(|candidate| candidate != nonce);
            let mut removed = state
                .registrations
                .remove(nonce)
                .map(|registration| vec![removed_registration(nonce.to_string(), registration)])
                .unwrap_or_default();
            removed.extend(remove_expired(&mut state, now));
            drop(state);
            self.finish_dropped_registrations(removed);
            return CapturedClaudeIngest::without_session(RelayIngestStatus::Expired);
        }
        let Some(_) = state.registrations.get(nonce) else {
            let removed = remove_expired(&mut state, now);
            drop(state);
            self.finish_dropped_registrations(removed);
            return CapturedClaudeIngest::without_session(RelayIngestStatus::Rejected);
        };
        let registration = state
            .registrations
            .get_mut(nonce)
            .expect("registration checked above");
        let stable_session_key = registration.stable_session_key.clone();
        registration.expires_at = now + self.limits.registration_ttl;
        let outcome = registration.reducer.apply_json(body, occurred_at_epoch_ms);
        let session_ended = outcome.drafts.iter().any(|draft| {
            matches!(&draft.kind, SemanticEventKind::Status { state, .. } if state == "ended")
        });
        let cleanup_paths = if session_ended {
            state.order.retain(|candidate| candidate != nonce);
            state
                .registrations
                .remove(nonce)
                .map(|registration| registration.cleanup_paths)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        drop(state);
        remove_cleanup_paths(cleanup_paths);
        CapturedClaudeIngest {
            status: RelayIngestStatus::Accepted(outcome),
            stable_session_key: Some(stable_session_key),
            session_ended,
            nonce: Some(nonce.to_string()),
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

    fn ingest_and_dispatch_at(
        &self,
        peer: SocketAddr,
        nonce: &str,
        body: &[u8],
        now: Instant,
        occurred_at_epoch_ms: u64,
    ) -> RelayIngestStatus {
        let captured = self.ingest_captured_at(peer, nonce, body, now, occurred_at_epoch_ms);
        self.dispatch_captured(captured)
    }

    fn dispatch_captured(&self, captured: CapturedClaudeIngest) -> RelayIngestStatus {
        let CapturedClaudeIngest {
            status,
            stable_session_key,
            session_ended,
            nonce,
        } = captured;
        let RelayIngestStatus::Accepted(outcome) = &status else {
            return status;
        };
        let Some(stable_session_key) = stable_session_key else {
            return status;
        };
        let handler = self
            .event_handler
            .read()
            .ok()
            .and_then(|handler| handler.clone());
        if let Some(handler) = handler.as_ref() {
            for draft in &outcome.drafts {
                invoke_registry_handler(handler, ClaudeRegistryEvent::Semantic(draft.clone()));
            }
            if outcome.degraded {
                invoke_registry_handler(
                    handler,
                    ClaudeRegistryEvent::AdapterHealth {
                        stable_session_key: stable_session_key.clone(),
                        health: SemanticAdapterHealth::Degraded,
                    },
                );
            }
        }
        if session_ended {
            if let Some(handler) = handler {
                invoke_registry_handler(
                    &handler,
                    ClaudeRegistryEvent::SessionEnded {
                        stable_session_key,
                        nonce: nonce.unwrap_or_default(),
                    },
                );
            }
        }
        status
    }

    pub fn unregister(&self, nonce: &str) -> Option<StableSessionKey> {
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        state.order.retain(|candidate| candidate != nonce);
        let registration = state.registrations.remove(nonce);
        drop(state);
        registration.map(|registration| {
            remove_cleanup_paths(registration.cleanup_paths);
            registration.stable_session_key
        })
    }

    pub fn cleanup_expired_at(&self, now: Instant) -> usize {
        let Ok(mut state) = self.state.lock() else {
            return 0;
        };
        let before = state.registrations.len();
        let removed_registrations = remove_expired(&mut state, now);
        let removed = before.saturating_sub(state.registrations.len());
        drop(state);
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
                    ClaudeRegistryEvent::RegistrationDropped {
                        stable_session_key: registration.stable_session_key,
                        nonce: registration.nonce,
                    },
                );
            }
        }
    }
}

fn invoke_registry_handler(handler: &ClaudeRegistryEventHandler, event: ClaudeRegistryEvent) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(event)));
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
    stable_session_key: Option<StableSessionKey>,
    session_ended: bool,
    nonce: Option<String>,
}

impl CapturedClaudeIngest {
    fn without_session(status: RelayIngestStatus) -> Self {
        Self {
            status,
            stable_session_key: None,
            session_ended: false,
            nonce: None,
        }
    }
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
        if let Some(registration) = state.registrations.remove(&nonce) {
            removed.push(removed_registration(nonce, registration));
        }
    }
    state
        .order
        .retain(|nonce| state.registrations.contains_key(nonce));
    removed
}

fn removed_registration(
    nonce: String,
    registration: RegisteredClaudeSession,
) -> RemovedClaudeRegistration {
    RemovedClaudeRegistration {
        nonce,
        stable_session_key: registration.stable_session_key,
        cleanup_paths: registration.cleanup_paths,
    }
}

fn remove_cleanup_paths(paths: Vec<PathBuf>) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

pub struct ClaudeHookRelayListener {
    endpoint: String,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ClaudeHookRelayListener {
    pub fn start(registry: Arc<ClaudeHookRegistry>) -> Result<Self, String> {
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
        let thread = thread::Builder::new()
            .name("claude-hook-relay".to_string())
            .spawn(move || {
                runtime.block_on(async move {
                    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
                        return;
                    };
                    let app = Router::new()
                        .route("/internal/claude-hook", post(handle_claude_hook))
                        .layer(DefaultBodyLimit::max(body_limit))
                        .with_state(registry);
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
            })
            .map_err(|error| format!("spawn Claude hook relay: {error}"))?;
        Ok(Self {
            endpoint,
            shutdown_tx: Some(shutdown_tx),
            thread: Some(thread),
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
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn handle_claude_hook(
    State(registry): State<Arc<ClaudeHookRegistry>>,
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
    match registry.ingest_and_dispatch_at(peer, nonce, &body, Instant::now(), unix_epoch_ms()) {
        RelayIngestStatus::Accepted(_) => StatusCode::NO_CONTENT,
        RelayIngestStatus::Rejected => StatusCode::UNAUTHORIZED,
        RelayIngestStatus::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        RelayIngestStatus::Expired => StatusCode::GONE,
    }
}

fn unix_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn random_nonce() -> Result<String, String> {
    let mut bytes = [0_u8; CLAUDE_NONCE_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| format!("generate Claude hook nonce: {error}"))?;
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
    let settings_path = temp_root.join(format!("claude-hooks-{}.json", registration.nonce));
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
        health: SemanticAdapterHealth::Healthy,
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
    fn accepted_ingest_keeps_dispatch_context_after_concurrent_unregister() {
        let registry = ClaudeHookRegistry::default();
        let registration = registry
            .register_at(StableSessionKey::from_tab("race-tab"), Instant::now())
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        registry.set_event_handler(Some(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        })));
        let captured = registry.ingest_captured_at(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 45000),
            &registration.nonce,
            br#"{"hook_event_name":"UserPromptSubmit","prompt":"race"}"#,
            Instant::now(),
            1_800_000_000_000,
        );

        // Models an unregister winning between relay ingestion and callback
        // dispatch. The accepted event owns all routing context it needs.
        registry.unregister(&registration.nonce);
        let status = registry.dispatch_captured(captured);

        assert!(matches!(status, RelayIngestStatus::Accepted(_)));
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            ClaudeRegistryEvent::Semantic(SemanticEventDraft {
                stable_session_key,
                kind: SemanticEventKind::UserMessage { text },
                ..
            }) if stable_session_key == &StableSessionKey::from_tab("race-tab") && text == "race"
        )));
    }
}
