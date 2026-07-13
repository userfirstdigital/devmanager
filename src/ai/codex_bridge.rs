use crate::remote::presentation::{
    SemanticEventDraft, SemanticEventKind, SemanticRetention, SemanticSource, SemanticStream,
    SemanticToolState, StableSessionKey,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::{
    accept_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};

const DEFAULT_ACTIVE_ITEMS: usize = 64;
const DEFAULT_ITEM_BYTES: usize = 64 * 1024;
const DEFAULT_TOTAL_BYTES: usize = 2 * 1024 * 1024;
const CODEX_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);
const MAX_PROBE_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_CODEX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const BRIDGE_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub struct CodexReducerLimits {
    pub active_items: usize,
    pub item_bytes: usize,
    pub total_bytes: usize,
}

impl Default for CodexReducerLimits {
    fn default() -> Self {
        Self {
            active_items: DEFAULT_ACTIVE_ITEMS,
            item_bytes: DEFAULT_ITEM_BYTES,
            total_bytes: DEFAULT_TOTAL_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexReducerUsage {
    pub active_items: usize,
    pub text_bytes: usize,
}

#[derive(Debug, Default)]
struct BufferedItem {
    text: String,
    completed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexProtocolState {
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
}

/// A tolerant, bounded projection of Codex app-server v2 messages.
///
/// This observer deliberately uses `serde_json::Value`: app-server remains the
/// protocol authority and newly-added fields or methods must never prevent the
/// underlying TUI connection from working.
pub struct CodexSemanticReducer {
    stable_session_key: StableSessionKey,
    limits: CodexReducerLimits,
    items: HashMap<String, BufferedItem>,
    item_order: VecDeque<String>,
    text_bytes: usize,
    protocol_state: CodexProtocolState,
}

impl CodexSemanticReducer {
    pub fn new(stable_session_key: StableSessionKey) -> Self {
        Self::with_limits(stable_session_key, CodexReducerLimits::default())
    }

    pub fn with_limits(
        stable_session_key: StableSessionKey,
        mut limits: CodexReducerLimits,
    ) -> Self {
        limits.active_items = limits.active_items.max(1);
        limits.item_bytes = limits.item_bytes.max(1);
        limits.total_bytes = limits.total_bytes.max(1);
        Self {
            stable_session_key,
            limits,
            items: HashMap::new(),
            item_order: VecDeque::new(),
            text_bytes: 0,
            protocol_state: CodexProtocolState::default(),
        }
    }

    pub fn memory_usage(&self) -> CodexReducerUsage {
        CodexReducerUsage {
            active_items: self.items.len(),
            text_bytes: self.text_bytes,
        }
    }

    pub fn protocol_state(&self) -> CodexProtocolState {
        self.protocol_state.clone()
    }

    pub fn observe_bytes(
        &mut self,
        bytes: &[u8],
        occurred_at_epoch_ms: u64,
    ) -> Vec<SemanticEventDraft> {
        let Ok(raw) = std::str::from_utf8(bytes) else {
            return Vec::new();
        };
        self.observe(raw, occurred_at_epoch_ms)
    }

    pub fn observe(&mut self, raw: &str, occurred_at_epoch_ms: u64) -> Vec<SemanticEventDraft> {
        let Ok(message) = serde_json::from_str::<Value>(raw) else {
            return Vec::new();
        };
        self.capture_protocol_state(&message);
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Vec::new();
        };
        let params = message.get("params").unwrap_or(&Value::Null);

        match method {
            "item/agentMessage/delta" => self.agent_delta(params, occurred_at_epoch_ms),
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                self.reasoning_delta(params, occurred_at_epoch_ms)
            }
            "item/plan/delta" => self.plan_delta(params, occurred_at_epoch_ms),
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                self.output_delta(params, occurred_at_epoch_ms)
            }
            "item/fileChange/patchUpdated" => self.patch_updated(params, occurred_at_epoch_ms),
            "item/started" => self.item_event(params, occurred_at_epoch_ms, false),
            "item/completed" => self.item_event(params, occurred_at_epoch_ms, true),
            "turn/diff/updated" => self.turn_diff(params, occurred_at_epoch_ms),
            "turn/plan/updated" => self.turn_plan(params, occurred_at_epoch_ms),
            "thread/started" => vec![self.event(
                occurred_at_epoch_ms,
                SemanticEventKind::Status {
                    state: "ready".to_string(),
                    detail: None,
                },
                SemanticRetention::Canonical,
                Some(format!(
                    "codex:thread:{}",
                    string_field(params, "threadId").unwrap_or("unknown")
                )),
            )],
            "thread/status/changed" => self.thread_status(params, occurred_at_epoch_ms),
            "turn/started" => self.turn_status(params, occurred_at_epoch_ms, "working"),
            "turn/completed" => self.turn_status(params, occurred_at_epoch_ms, "idle"),
            "error" => self.error_event(params, occurred_at_epoch_ms),
            "item/commandExecution/requestApproval" => {
                self.approval_question(&message, params, occurred_at_epoch_ms, "Command approval")
            }
            "item/fileChange/requestApproval" => self.approval_question(
                &message,
                params,
                occurred_at_epoch_ms,
                "File change approval",
            ),
            "item/permissions/requestApproval" => self.approval_question(
                &message,
                params,
                occurred_at_epoch_ms,
                "Permission approval",
            ),
            "item/tool/requestUserInput" => {
                self.user_input_questions(&message, params, occurred_at_epoch_ms)
            }
            _ => Vec::new(),
        }
    }

    fn agent_delta(&mut self, params: &Value, now: u64) -> Vec<SemanticEventDraft> {
        let Some(item_id) = string_field(params, "itemId") else {
            return Vec::new();
        };
        let Some(delta) = string_field(params, "delta") else {
            return Vec::new();
        };
        let state_key = format!("assistant:{item_id}");
        let Some(text) = self.append_item(&state_key, delta) else {
            return Vec::new();
        };
        vec![self.assistant_event(now, item_id, text, true)]
    }

    fn reasoning_delta(&mut self, params: &Value, now: u64) -> Vec<SemanticEventDraft> {
        let Some(item_id) = string_field(params, "itemId") else {
            return Vec::new();
        };
        let Some(delta) = string_field(params, "delta") else {
            return Vec::new();
        };
        let state_key = format!("reasoning:{item_id}");
        let Some(summary) = self.append_item(&state_key, delta) else {
            return Vec::new();
        };
        vec![self.event(
            now,
            SemanticEventKind::Reasoning {
                item_id: item_id.to_string(),
                summary,
            },
            SemanticRetention::Verbose,
            Some(format!("codex:reasoning:{item_id}")),
        )]
    }

    fn plan_delta(&mut self, params: &Value, now: u64) -> Vec<SemanticEventDraft> {
        let Some(item_id) = string_field(params, "itemId") else {
            return Vec::new();
        };
        let Some(delta) = string_field(params, "delta") else {
            return Vec::new();
        };
        let state_key = format!("plan:{item_id}");
        let Some(summary) = self.append_item(&state_key, delta) else {
            return Vec::new();
        };
        vec![self.tool_event(
            now,
            item_id,
            "Plan",
            SemanticToolState::Running,
            summary,
            format!("codex:plan-item:{item_id}"),
        )]
    }

    fn output_delta(&mut self, params: &Value, now: u64) -> Vec<SemanticEventDraft> {
        let Some(item_id) = string_field(params, "itemId") else {
            return Vec::new();
        };
        let Some(delta) = string_field(params, "delta") else {
            return Vec::new();
        };
        let state_key = format!("output:{item_id}");
        let Some(text) = self.append_item(&state_key, delta) else {
            return Vec::new();
        };
        vec![self.event(
            now,
            SemanticEventKind::Output {
                stream: SemanticStream::Stdout,
                text,
            },
            SemanticRetention::Verbose,
            Some(format!("codex:output:{item_id}")),
        )]
    }

    fn patch_updated(&mut self, params: &Value, now: u64) -> Vec<SemanticEventDraft> {
        let Some(item_id) = string_field(params, "itemId") else {
            return Vec::new();
        };
        let Some(changes) = params.get("changes").and_then(Value::as_array) else {
            return Vec::new();
        };
        let diff = changes
            .iter()
            .filter_map(|change| change.get("diff").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if diff.is_empty() {
            return Vec::new();
        }
        vec![self.event(
            now,
            SemanticEventKind::Diff {
                item_id: item_id.to_string(),
                unified_diff: self.visible_text(&diff),
            },
            SemanticRetention::Canonical,
            Some(format!("codex:file-change:{item_id}")),
        )]
    }

    fn item_event(&mut self, params: &Value, now: u64, completed: bool) -> Vec<SemanticEventDraft> {
        let Some(item) = params.get("item") else {
            return Vec::new();
        };
        let Some(item_type) = string_field(item, "type") else {
            return Vec::new();
        };
        let Some(item_id) = string_field(item, "id") else {
            return Vec::new();
        };

        match item_type {
            "userMessage" => {
                if !completed {
                    return Vec::new();
                }
                let text = user_message_text(item);
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![self.event(
                        now,
                        SemanticEventKind::UserMessage {
                            text: self.visible_text(&text),
                        },
                        SemanticRetention::Canonical,
                        Some(format!("codex:user:{item_id}")),
                    )]
                }
            }
            "agentMessage" => {
                let text = string_field(item, "text").unwrap_or_default();
                if text.is_empty() && !completed {
                    return Vec::new();
                }
                let state_key = format!("assistant:{item_id}");
                let text = self.complete_or_replace_item(&state_key, text, completed);
                vec![self.assistant_event(now, item_id, text, !completed)]
            }
            "reasoning" => {
                let summary = string_array(item.get("summary"));
                if summary.is_empty() {
                    return Vec::new();
                }
                let state_key = format!("reasoning:{item_id}");
                let summary = self.complete_or_replace_item(&state_key, &summary, completed);
                vec![self.event(
                    now,
                    SemanticEventKind::Reasoning {
                        item_id: item_id.to_string(),
                        summary,
                    },
                    SemanticRetention::Verbose,
                    Some(format!("codex:reasoning:{item_id}")),
                )]
            }
            "plan" => {
                let text = string_field(item, "text").unwrap_or_default();
                let state_key = format!("plan:{item_id}");
                let text = self.complete_or_replace_item(&state_key, text, completed);
                vec![self.tool_event(
                    now,
                    item_id,
                    "Plan",
                    if completed {
                        SemanticToolState::Completed
                    } else {
                        SemanticToolState::Running
                    },
                    text,
                    format!("codex:plan-item:{item_id}"),
                )]
            }
            "commandExecution" => {
                let command = self.visible_text(string_field(item, "command").unwrap_or("Command"));
                let exit_code = item
                    .get("exitCode")
                    .and_then(Value::as_i64)
                    .and_then(|code| i32::try_from(code).ok());
                vec![self.event(
                    now,
                    SemanticEventKind::Command {
                        command_id: item_id.to_string(),
                        text: command,
                        exit_code,
                    },
                    SemanticRetention::Canonical,
                    Some(format!("codex:command:{item_id}")),
                )]
            }
            "fileChange" => {
                let count = item
                    .get("changes")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                let failed = string_field(item, "status") == Some("failed");
                vec![self.tool_event(
                    now,
                    item_id,
                    "File changes",
                    if failed {
                        SemanticToolState::Failed
                    } else if completed {
                        SemanticToolState::Completed
                    } else {
                        SemanticToolState::Running
                    },
                    format!("{count} file{}", if count == 1 { "" } else { "s" }),
                    format!("codex:file-change-status:{item_id}"),
                )]
            }
            other if other.ends_with("ToolCall") || other == "webSearch" => {
                let name = tool_name(item, other);
                let summary = tool_summary(item);
                let state = tool_state(string_field(item, "status"), completed);
                vec![self.tool_event(
                    now,
                    item_id,
                    &name,
                    state,
                    self.visible_text(&summary),
                    format!("codex:tool:{item_id}"),
                )]
            }
            _ => Vec::new(),
        }
    }

    fn turn_diff(&self, params: &Value, now: u64) -> Vec<SemanticEventDraft> {
        let Some(diff) = string_field(params, "diff") else {
            return Vec::new();
        };
        let turn_id = string_field(params, "turnId").unwrap_or("unknown");
        vec![self.event(
            now,
            SemanticEventKind::Diff {
                item_id: turn_id.to_string(),
                unified_diff: self.visible_text(diff),
            },
            SemanticRetention::Canonical,
            Some(format!("codex:turn-diff:{turn_id}")),
        )]
    }

    fn turn_plan(&self, params: &Value, now: u64) -> Vec<SemanticEventDraft> {
        let Some(plan) = params.get("plan").and_then(Value::as_array) else {
            return Vec::new();
        };
        let turn_id = string_field(params, "turnId").unwrap_or("unknown");
        let mut any_running = false;
        let mut any_pending = false;
        let summary = plan
            .iter()
            .filter_map(|step| {
                let text = string_field(step, "step")?;
                let status = string_field(step, "status").unwrap_or("pending");
                any_running |= status == "inProgress";
                any_pending |= status == "pending";
                let marker = match status {
                    "completed" => "[done]",
                    "inProgress" => "[now]",
                    _ => "[next]",
                };
                Some(format!("{marker} {text}"))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let state = if any_running {
            SemanticToolState::Running
        } else if any_pending {
            SemanticToolState::Pending
        } else {
            SemanticToolState::Completed
        };
        vec![self.tool_event(
            now,
            turn_id,
            "Plan",
            state,
            self.visible_text(&summary),
            format!("codex:turn-plan:{turn_id}"),
        )]
    }

    fn thread_status(&self, params: &Value, now: u64) -> Vec<SemanticEventDraft> {
        let status = params.get("status").unwrap_or(&Value::Null);
        let state = string_field(status, "type")
            .unwrap_or("unknown")
            .to_string();
        let detail = status
            .get("activeFlags")
            .and_then(Value::as_array)
            .map(|flags| {
                flags
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|text| !text.is_empty());
        let thread_id = string_field(params, "threadId").unwrap_or("unknown");
        vec![self.event(
            now,
            SemanticEventKind::Status { state, detail },
            SemanticRetention::Canonical,
            Some(format!("codex:thread-status:{thread_id}")),
        )]
    }

    fn turn_status(&self, params: &Value, now: u64, state: &str) -> Vec<SemanticEventDraft> {
        let turn_id = string_field(params, "turnId")
            .or_else(|| params.get("turn").and_then(|turn| string_field(turn, "id")))
            .unwrap_or("unknown");
        vec![self.event(
            now,
            SemanticEventKind::Status {
                state: state.to_string(),
                detail: None,
            },
            SemanticRetention::Canonical,
            Some(format!("codex:turn-status:{turn_id}")),
        )]
    }

    fn error_event(&self, params: &Value, now: u64) -> Vec<SemanticEventDraft> {
        let Some(message) = params
            .get("error")
            .and_then(|error| string_field(error, "message"))
        else {
            return Vec::new();
        };
        let suffix = if params.get("willRetry").and_then(Value::as_bool) == Some(true) {
            " Retrying."
        } else {
            ""
        };
        let turn_id = string_field(params, "turnId").unwrap_or("unknown");
        vec![self.event(
            now,
            SemanticEventKind::Error {
                message: self.visible_text(&format!("{message}{suffix}")),
            },
            SemanticRetention::Canonical,
            Some(format!("codex:error:{turn_id}")),
        )]
    }

    fn approval_question(
        &self,
        message: &Value,
        params: &Value,
        now: u64,
        fallback: &str,
    ) -> Vec<SemanticEventDraft> {
        let request_id = rpc_id(message).unwrap_or_else(|| "unknown".to_string());
        let item_id = string_field(params, "itemId").unwrap_or("unknown");
        let reason = string_field(params, "reason").unwrap_or(fallback);
        let prompt = match string_field(params, "command") {
            Some(command) => format!("{reason}\n\n{command}"),
            None => reason.to_string(),
        };
        let question_id = format!("codex:{request_id}:{item_id}");
        vec![self.event(
            now,
            SemanticEventKind::Question {
                question_id: question_id.clone(),
                prompt: self.visible_text(&prompt),
                choices: vec!["Approve".to_string(), "Decline".to_string()],
            },
            SemanticRetention::Canonical,
            Some(format!("codex:question:{question_id}")),
        )]
    }

    fn user_input_questions(
        &self,
        message: &Value,
        params: &Value,
        now: u64,
    ) -> Vec<SemanticEventDraft> {
        let request_id = rpc_id(message).unwrap_or_else(|| "unknown".to_string());
        let item_id = string_field(params, "itemId").unwrap_or("unknown");
        params
            .get("questions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|question| {
                let local_id = string_field(question, "id")?;
                let prompt = string_field(question, "question")?;
                let choices = question
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|option| string_field(option, "label").map(str::to_string))
                    .take(32)
                    .collect::<Vec<_>>();
                let question_id = format!("codex:{request_id}:{item_id}:{local_id}");
                Some(self.event(
                    now,
                    SemanticEventKind::Question {
                        question_id: question_id.clone(),
                        prompt: self.visible_text(prompt),
                        choices,
                    },
                    SemanticRetention::Canonical,
                    Some(format!("codex:question:{question_id}")),
                ))
            })
            .collect()
    }

    fn assistant_event(
        &self,
        now: u64,
        item_id: &str,
        text: String,
        streaming: bool,
    ) -> SemanticEventDraft {
        self.event(
            now,
            SemanticEventKind::AssistantMessage {
                message_id: item_id.to_string(),
                text,
                streaming,
            },
            SemanticRetention::Canonical,
            Some(format!("codex:assistant:{item_id}")),
        )
    }

    fn tool_event(
        &self,
        now: u64,
        item_id: &str,
        name: &str,
        state: SemanticToolState,
        summary: String,
        deduplication_key: String,
    ) -> SemanticEventDraft {
        self.event(
            now,
            SemanticEventKind::Tool {
                tool_id: item_id.to_string(),
                name: name.to_string(),
                state,
                summary,
            },
            SemanticRetention::Canonical,
            Some(deduplication_key),
        )
    }

    fn event(
        &self,
        occurred_at_epoch_ms: u64,
        kind: SemanticEventKind,
        retention: SemanticRetention,
        deduplication_key: Option<String>,
    ) -> SemanticEventDraft {
        SemanticEventDraft {
            stable_session_key: self.stable_session_key.clone(),
            occurred_at_epoch_ms,
            source: SemanticSource::Codex,
            kind,
            retention,
            deduplication_key,
        }
    }

    fn append_item(&mut self, key: &str, delta: &str) -> Option<String> {
        self.ensure_item(key);
        if self.items.get(key).is_some_and(|item| item.completed) {
            return None;
        }
        let old_len = self.items.get(key).map_or(0, |item| item.text.len());
        let remaining = self.limits.item_bytes.saturating_sub(old_len);
        let addition = truncate_utf8(delta, remaining);
        if let Some(item) = self.items.get_mut(key) {
            item.text.push_str(addition);
        }
        self.text_bytes = self.text_bytes.saturating_add(addition.len());
        self.enforce_total_limit(key);
        self.items.get(key).map(|item| item.text.clone())
    }

    fn complete_or_replace_item(&mut self, key: &str, text: &str, completed: bool) -> String {
        self.ensure_item(key);
        let max_bytes = self.limits.item_bytes.min(self.limits.total_bytes);
        let bounded = truncate_utf8(text, max_bytes).to_string();
        if let Some(item) = self.items.get_mut(key) {
            self.text_bytes = self.text_bytes.saturating_sub(item.text.len());
            item.text = bounded;
            item.completed = completed;
            self.text_bytes = self.text_bytes.saturating_add(item.text.len());
        }
        self.enforce_total_limit(key);
        self.items
            .get(key)
            .map(|item| item.text.clone())
            .unwrap_or_default()
    }

    fn ensure_item(&mut self, key: &str) {
        if self.items.contains_key(key) {
            return;
        }
        while self.items.len() >= self.limits.active_items {
            if !self.evict_oldest_except(key) {
                break;
            }
        }
        self.items.insert(key.to_string(), BufferedItem::default());
        self.item_order.push_back(key.to_string());
    }

    fn enforce_total_limit(&mut self, protected_key: &str) {
        while self.text_bytes > self.limits.total_bytes {
            if !self.evict_oldest_except(protected_key) {
                break;
            }
        }
        if self.text_bytes > self.limits.total_bytes {
            if let Some(item) = self.items.get_mut(protected_key) {
                let old_len = item.text.len();
                item.text = truncate_utf8(&item.text, self.limits.total_bytes).to_string();
                self.text_bytes = self
                    .text_bytes
                    .saturating_sub(old_len)
                    .saturating_add(item.text.len());
            }
        }
    }

    fn evict_oldest_except(&mut self, protected_key: &str) -> bool {
        let scans = self.item_order.len();
        for _ in 0..scans {
            let Some(candidate) = self.item_order.pop_front() else {
                return false;
            };
            if candidate == protected_key {
                self.item_order.push_back(candidate);
                continue;
            }
            if let Some(item) = self.items.remove(&candidate) {
                self.text_bytes = self.text_bytes.saturating_sub(item.text.len());
                return true;
            }
        }
        false
    }

    fn visible_text(&self, text: &str) -> String {
        truncate_utf8(
            &sanitize_text(text),
            self.limits.item_bytes.min(self.limits.total_bytes),
        )
        .to_string()
    }

    fn capture_protocol_state(&mut self, message: &Value) {
        let params = message.get("params").unwrap_or(&Value::Null);
        let result = message.get("result").unwrap_or(&Value::Null);
        if let Some(thread_id) = string_field(params, "threadId")
            .or_else(|| {
                params
                    .get("thread")
                    .and_then(|value| string_field(value, "id"))
            })
            .or_else(|| string_field(result, "threadId"))
            .or_else(|| {
                result
                    .get("thread")
                    .and_then(|value| string_field(value, "id"))
            })
        {
            self.protocol_state.thread_id = Some(bounded_identifier(thread_id));
        }
        if let Some(turn_id) = string_field(params, "turnId")
            .or_else(|| {
                params
                    .get("turn")
                    .and_then(|value| string_field(value, "id"))
            })
            .or_else(|| string_field(result, "turnId"))
            .or_else(|| {
                result
                    .get("turn")
                    .and_then(|value| string_field(value, "id"))
            })
        {
            self.protocol_state.turn_id = Some(bounded_identifier(turn_id));
        }
        if let Some(item_id) = string_field(params, "itemId")
            .or_else(|| {
                params
                    .get("item")
                    .and_then(|value| string_field(value, "id"))
            })
            .or_else(|| string_field(result, "itemId"))
            .or_else(|| {
                result
                    .get("item")
                    .and_then(|value| string_field(value, "id"))
            })
        {
            self.protocol_state.item_id = Some(bounded_identifier(item_id));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedServerFrame {
    pub bytes: Vec<u8>,
    pub occurred_at_epoch_ms: u64,
}

const DEFAULT_OBSERVER_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug)]
struct ObserverBudget {
    queued_bytes: AtomicUsize,
    max_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct SemanticObserverSender {
    sender: SyncSender<ObservedServerFrame>,
    budget: std::sync::Arc<ObserverBudget>,
}

impl SemanticObserverSender {
    pub fn queued_bytes(&self) -> usize {
        self.budget.queued_bytes.load(Ordering::Acquire)
    }

    fn try_send(&self, frame: ObservedServerFrame) {
        let bytes = frame.bytes.len();
        if !reserve_observer_bytes(&self.budget, bytes) {
            return;
        }
        if self.sender.try_send(frame).is_err() {
            self.budget.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
        }
    }
}

#[derive(Debug)]
pub struct SemanticObserverReceiver {
    receiver: Receiver<ObservedServerFrame>,
    budget: std::sync::Arc<ObserverBudget>,
}

impl SemanticObserverReceiver {
    pub fn recv(&self) -> Result<ObservedServerFrame, std::sync::mpsc::RecvError> {
        let frame = self.receiver.recv()?;
        self.release(&frame);
        Ok(frame)
    }

    pub fn try_recv(&self) -> Result<ObservedServerFrame, TryRecvError> {
        let frame = self.receiver.try_recv()?;
        self.release(&frame);
        Ok(frame)
    }

    fn release(&self, frame: &ObservedServerFrame) {
        self.budget
            .queued_bytes
            .fetch_sub(frame.bytes.len(), Ordering::AcqRel);
    }
}

pub fn semantic_observer_channel(
    capacity: usize,
) -> (SemanticObserverSender, SemanticObserverReceiver) {
    semantic_observer_channel_with_limits(capacity, DEFAULT_OBSERVER_BYTES)
}

pub fn semantic_observer_channel_with_limits(
    capacity: usize,
    max_bytes: usize,
) -> (SemanticObserverSender, SemanticObserverReceiver) {
    let (sender, receiver) = sync_channel(capacity.max(1));
    let budget = std::sync::Arc::new(ObserverBudget {
        queued_bytes: AtomicUsize::new(0),
        max_bytes: max_bytes.max(1),
    });
    (
        SemanticObserverSender {
            sender,
            budget: budget.clone(),
        },
        SemanticObserverReceiver { receiver, budget },
    )
}

/// Offers a copy to the semantic observer but returns the original frame.
/// A full or disconnected observer is intentionally ignored.
pub fn forward_server_frame<'a>(
    frame: &'a [u8],
    occurred_at_epoch_ms: u64,
    observer: &SemanticObserverSender,
) -> &'a [u8] {
    observer.try_send(ObservedServerFrame {
        bytes: frame.to_vec(),
        occurred_at_epoch_ms,
    });
    frame
}

fn reserve_observer_bytes(budget: &ObserverBudget, bytes: usize) -> bool {
    let mut current = budget.queued_bytes.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(bytes) else {
            return false;
        };
        if next > budget.max_bytes {
            return false;
        }
        match budget.queued_bytes.compare_exchange_weak(
            current,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

pub fn peer_is_allowed(peer: SocketAddr) -> bool {
    peer.ip().is_loopback()
}

/// Serves exactly one TUI WebSocket and transparently connects it to one
/// app-server stdio stream. JSONL delimiters are transport framing; every byte
/// inside a frame is otherwise preserved.
pub async fn serve_one_loopback_client<S>(
    listener: TcpListener,
    stdio: S,
    observer: SemanticObserverSender,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (socket, peer) = tokio::select! {
        accepted = listener.accept() => accepted.map_err(|error| format!("Codex bridge accept failed: {error}"))?,
        _ = &mut shutdown => return Ok(()),
    };
    if !peer_is_allowed(peer) {
        return Err(format!("Codex bridge rejected non-loopback peer {peer}"));
    }

    let websocket_config = WebSocketConfig {
        max_message_size: Some(MAX_CODEX_FRAME_BYTES),
        max_frame_size: Some(MAX_CODEX_FRAME_BYTES),
        max_write_buffer_size: MAX_CODEX_FRAME_BYTES * 2,
        ..WebSocketConfig::default()
    };
    let websocket = tokio::select! {
        result = tokio::time::timeout(BRIDGE_IO_TIMEOUT, accept_async_with_config(socket, Some(websocket_config))) => {
            result
                .map_err(|_| "Codex bridge WebSocket handshake timed out".to_string())?
                .map_err(|error| format!("Codex bridge WebSocket handshake failed: {error}"))?
        },
        _ = &mut shutdown => return Ok(()),
    };
    let (mut websocket_write, mut websocket_read) = websocket.split();
    let (stdio_read, mut stdio_write) = tokio::io::split(stdio);
    let mut stdio_read = BufReader::new(stdio_read);
    let mut server_frame = Vec::new();

    loop {
        server_frame.clear();
        tokio::select! {
            read = read_jsonl_frame(&mut stdio_read, &mut server_frame, MAX_CODEX_FRAME_BYTES) => {
                let read = read.map_err(|error| format!("Codex app-server read failed: {error}"))?;
                if read == 0 {
                    let _ = websocket_write.send(Message::Close(None)).await;
                    return Ok(());
                }
                let observed_at = epoch_millis();
                let frame = forward_server_frame(&server_frame, observed_at, &observer);
                let message = match std::str::from_utf8(frame) {
                    Ok(text) => Message::Text(text.to_string().into()),
                    Err(_) => Message::Binary(frame.to_vec()),
                };
                bridge_io("WebSocket send", websocket_write.send(message)).await?;
            }
            incoming = websocket_read.next() => {
                let Some(incoming) = incoming else {
                    return Ok(());
                };
                let incoming = incoming
                    .map_err(|error| format!("Codex bridge WebSocket read failed: {error}"))?;
                match incoming {
                    Message::Text(text) => {
                        bridge_io("app-server write", async {
                            stdio_write.write_all(text.as_bytes()).await?;
                            stdio_write.write_all(b"\n").await?;
                            stdio_write.flush().await
                        }).await?;
                    }
                    Message::Binary(bytes) => {
                        bridge_io("app-server write", async {
                            stdio_write.write_all(&bytes).await?;
                            stdio_write.write_all(b"\n").await?;
                            stdio_write.flush().await
                        }).await?;
                    }
                    Message::Ping(payload) => {
                        bridge_io("WebSocket pong", websocket_write.send(Message::Pong(payload))).await?;
                    }
                    Message::Close(_) => return Ok(()),
                    Message::Pong(_) | Message::Frame(_) => {}
                }
            }
            _ = &mut shutdown => {
                let _ = websocket_write.send(Message::Close(None)).await;
                return Ok(());
            }
        }
    }
}

async fn bridge_io<T, E, F>(label: &str, future: F) -> Result<T, String>
where
    E: std::fmt::Display,
    F: std::future::Future<Output = Result<T, E>>,
{
    tokio::time::timeout(BRIDGE_IO_TIMEOUT, future)
        .await
        .map_err(|_| format!("Codex bridge {label} timed out"))?
        .map_err(|error| format!("Codex bridge {label} failed: {error}"))
}

async fn read_jsonl_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    frame: &mut Vec<u8>,
    max_bytes: usize,
) -> Result<usize, String> {
    frame.clear();
    loop {
        let available = reader.fill_buf().await.map_err(|error| error.to_string())?;
        if available.is_empty() {
            return Ok(frame.len());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        let payload = newline.map_or(available, |position| &available[..position]);
        let remaining = max_bytes.saturating_sub(frame.len());
        if payload.len() > remaining {
            frame.extend_from_slice(&payload[..remaining]);
            reader.consume(consumed);
            return Err(format!(
                "JSONL frame exceeded the {max_bytes}-byte bridge limit"
            ));
        }
        frame.extend_from_slice(payload);
        reader.consume(consumed);
        if newline.is_some() {
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(frame.len());
        }
    }
}

fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Debug, Clone)]
pub struct PreparedCodexAdapter {
    executable: PathBuf,
    sidecar_prefix_args: Vec<String>,
    tui_args: Vec<String>,
    version: String,
}

#[derive(Debug)]
pub struct CodexBridgeHandle {
    endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
    bridge_thread: Option<std::thread::JoinHandle<()>>,
    observer_thread: Option<std::thread::JoinHandle<()>>,
}

impl CodexBridgeHandle {
    pub fn start<OnEvent, OnExit>(
        prepared: PreparedCodexAdapter,
        cwd: PathBuf,
        stable_session_key: StableSessionKey,
        on_event: OnEvent,
        on_exit: OnExit,
    ) -> Result<Self, String>
    where
        OnEvent: Fn(SemanticEventDraft) + Send + Sync + 'static,
        OnExit: Fn(String) + Send + Sync + 'static,
    {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| format!("Cannot bind Codex loopback bridge: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("Cannot configure Codex loopback bridge: {error}"))?;
        let endpoint = format!(
            "ws://{}",
            listener
                .local_addr()
                .map_err(|error| format!("Cannot inspect Codex loopback bridge: {error}"))?
        );
        let (observer, receiver) = semantic_observer_channel(256);
        let on_event = std::sync::Arc::new(on_event);
        let observer_thread = std::thread::Builder::new()
            .name("codex-semantic-observer".to_string())
            .spawn(move || {
                let mut reducer = CodexSemanticReducer::new(stable_session_key);
                while let Ok(frame) = receiver.recv() {
                    for event in reducer.observe_bytes(&frame.bytes, frame.occurred_at_epoch_ms) {
                        on_event(event);
                    }
                }
            })
            .map_err(|error| format!("Cannot start Codex semantic observer: {error}"))?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let on_exit = std::sync::Arc::new(on_exit);
        let bridge_thread = match std::thread::Builder::new()
            .name("codex-app-server-bridge".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx
                            .send(Err(format!("Cannot start Codex bridge runtime: {error}")));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let listener = match TcpListener::from_std(listener) {
                        Ok(listener) => listener,
                        Err(error) => {
                            let _ = ready_tx.send(Err(format!(
                                "Cannot adopt Codex loopback listener: {error}"
                            )));
                            return;
                        }
                    };
                    let mut command = tokio::process::Command::new(prepared.executable());
                    command
                        .args(prepared.sidecar_args())
                        .current_dir(cwd)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .kill_on_drop(true);
                    #[cfg(windows)]
                    command.creation_flags(0x0800_0000);

                    let mut child = match command.spawn() {
                        Ok(child) => child,
                        Err(error) => {
                            let _ = ready_tx
                                .send(Err(format!("Failed to start Codex app-server: {error}")));
                            return;
                        }
                    };
                    if let Ok(Some(status)) = child.try_wait() {
                        let _ = ready_tx.send(Err(format!(
                            "Codex app-server exited during startup with {status}"
                        )));
                        return;
                    }
                    let Some(stdin) = child.stdin.take() else {
                        let _ =
                            ready_tx.send(Err("Codex app-server did not expose stdin".to_string()));
                        terminate_sidecar(&mut child).await;
                        return;
                    };
                    let Some(stdout) = child.stdout.take() else {
                        let _ = ready_tx
                            .send(Err("Codex app-server did not expose stdout".to_string()));
                        terminate_sidecar(&mut child).await;
                        return;
                    };
                    let stderr_task = child.stderr.take().map(|stderr| {
                        tokio::spawn(async move {
                            let mut stderr = BufReader::new(stderr);
                            let mut buffer = [0_u8; 8 * 1024];
                            loop {
                                match tokio::io::AsyncReadExt::read(&mut stderr, &mut buffer).await
                                {
                                    Ok(0) | Err(_) => break,
                                    Ok(_) => {}
                                }
                            }
                        })
                    });
                    let stdio = JoinedStdio {
                        reader: stdout,
                        writer: stdin,
                    };
                    let _ = ready_tx.send(Ok(()));

                    let bridge = serve_one_loopback_client(listener, stdio, observer, shutdown_rx);
                    tokio::pin!(bridge);
                    let outcome = tokio::select! {
                        result = &mut bridge => result,
                        status = child.wait() => match status {
                            Ok(status) => Err(format!("Codex app-server exited with {status}")),
                            Err(error) => Err(format!("Codex app-server wait failed: {error}")),
                        },
                    };
                    terminate_sidecar(&mut child).await;
                    if let Some(task) = stderr_task {
                        task.abort();
                        let _ = task.await;
                    }
                    if let Err(error) = outcome {
                        on_exit(error);
                    }
                });
            }) {
            Ok(thread) => thread,
            Err(error) => {
                drop(shutdown_tx);
                let _ = observer_thread.join();
                return Err(format!("Cannot start Codex bridge thread: {error}"));
            }
        };

        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                endpoint,
                shutdown: Some(shutdown_tx),
                bridge_thread: Some(bridge_thread),
                observer_thread: Some(observer_thread),
            }),
            Ok(Err(error)) => {
                drop(shutdown_tx);
                let _ = bridge_thread.join();
                let _ = observer_thread.join();
                Err(error)
            }
            Err(_) => {
                drop(shutdown_tx);
                let _ = bridge_thread.join();
                let _ = observer_thread.join();
                Err("Codex app-server startup did not become ready in time".to_string())
            }
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.bridge_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.observer_thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CodexBridgeHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct JoinedStdio<R, W> {
    reader: R,
    writer: W,
}

impl<R: AsyncRead + Unpin, W: Unpin> AsyncRead for JoinedStdio<R, W> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.reader).poll_read(context, buffer)
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> AsyncWrite for JoinedStdio<R, W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        std::pin::Pin::new(&mut self.writer).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.writer).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.writer).poll_shutdown(context)
    }
}

async fn terminate_sidecar(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        let _ = tokio::task::spawn_blocking(move || {
            crate::services::platform_service::kill_process_tree(pid)
        })
        .await;
    }
    let _ = child.kill().await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await;
}

impl PreparedCodexAdapter {
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn sidecar_args(&self) -> Vec<String> {
        let mut args = self.sidecar_prefix_args.clone();
        args.extend([
            "app-server".to_string(),
            "--listen".to_string(),
            "stdio://".to_string(),
        ]);
        args
    }

    pub fn tui_command(&self, endpoint: &str, shell_program: &str) -> String {
        let mut tokens = Vec::with_capacity(self.tui_args.len() + 3);
        tokens.push(self.executable.to_string_lossy().into_owned());
        tokens.extend(self.tui_args.iter().cloned());
        tokens.push("--remote".to_string());
        tokens.push(endpoint.to_string());
        quote_command_for_shell(&tokens, shell_program)
    }
}

#[derive(Debug)]
enum ParsedCodexCommand {
    Npx {
        package_index: usize,
        tokens: Vec<String>,
        requested_version: String,
    },
    Direct {
        tokens: Vec<String>,
    },
}

pub fn prepare_codex_adapter(startup_command: &str) -> Result<PreparedCodexAdapter, String> {
    prepare_codex_adapter_with(startup_command, resolve_executable, run_probe)
}

fn prepare_codex_adapter_with<Resolve, Probe>(
    startup_command: &str,
    mut resolve: Resolve,
    mut probe: Probe,
) -> Result<PreparedCodexAdapter, String>
where
    Resolve: FnMut(&str) -> Result<PathBuf, String>,
    Probe: FnMut(&Path, &[String]) -> Result<String, String>,
{
    let parsed = parse_codex_command(startup_command)?;
    let (executable_name, mut sidecar_prefix_args, mut tui_args, requested_version) = match parsed {
        ParsedCodexCommand::Npx {
            package_index,
            tokens,
            requested_version,
        } => {
            let executable_name = tokens[0].clone();
            let prefix = tokens[1..=package_index].to_vec();
            let tui_args = tokens[1..].to_vec();
            (executable_name, prefix, tui_args, Some(requested_version))
        }
        ParsedCodexCommand::Direct { tokens } => {
            let executable_name = tokens[0].clone();
            (executable_name, Vec::new(), tokens[1..].to_vec(), None)
        }
    };
    let executable = resolve(&executable_name)?;

    let version = if let Some(requested_version) = requested_version {
        if requested_version == "latest" {
            let mut args = sidecar_prefix_args.clone();
            args.push("--version".to_string());
            parse_codex_version(&probe(&executable, &args)?)?
        } else {
            requested_version
        }
    } else {
        parse_codex_version(&probe(&executable, &["--version".to_string()])?)?
    };
    validate_version_token(&version)?;

    if !sidecar_prefix_args.is_empty() {
        let exact_package = format!("@openai/codex@{version}");
        let package_offset = sidecar_prefix_args
            .iter()
            .position(|token| token.starts_with("@openai/codex@"))
            .ok_or_else(|| "Recognized Codex package disappeared during preparation".to_string())?;
        sidecar_prefix_args[package_offset] = exact_package.clone();
        let tui_package_offset = tui_args
            .iter()
            .position(|token| token.starts_with("@openai/codex@"))
            .ok_or_else(|| "Recognized Codex package disappeared during preparation".to_string())?;
        tui_args[tui_package_offset] = exact_package;

        let mut exact_version_args = sidecar_prefix_args.clone();
        exact_version_args.push("--version".to_string());
        let exact_version = parse_codex_version(&probe(&executable, &exact_version_args)?)?;
        if exact_version != version {
            return Err(format!(
                "Pinned Codex package reported version {exact_version}, expected {version}"
            ));
        }
    }

    let mut tui_help_args = sidecar_prefix_args.clone();
    tui_help_args.push("--help".to_string());
    let tui_help = probe(&executable, &tui_help_args)?;
    if !tui_help.contains("--remote") {
        return Err(format!(
            "Codex {version} does not advertise the required --remote capability"
        ));
    }

    let mut app_server_help_args = sidecar_prefix_args.clone();
    app_server_help_args.extend(["app-server".to_string(), "--help".to_string()]);
    let app_server_help = probe(&executable, &app_server_help_args)?;
    if !app_server_help.contains("--listen") {
        return Err(format!(
            "Codex {version} does not advertise the required app-server --listen capability"
        ));
    }

    Ok(PreparedCodexAdapter {
        executable,
        sidecar_prefix_args,
        tui_args,
        version,
    })
}

fn parse_codex_command(startup_command: &str) -> Result<ParsedCodexCommand, String> {
    let tokens = split_command_line(startup_command)?;
    let Some(first) = tokens.first() else {
        return Err("Codex command is empty".to_string());
    };
    let stem = Path::new(first)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(first)
        .to_ascii_lowercase();

    if stem == "npx" {
        let package_index = tokens
            .iter()
            .position(|token| token.starts_with("@openai/codex@"))
            .ok_or_else(|| {
                "Custom npx wrapper is not a recognized @openai/codex command".to_string()
            })?;
        if package_index == 0
            || tokens[1..package_index]
                .iter()
                .any(|token| token != "-y" && token != "--yes")
        {
            return Err("Custom npx wrapper options cannot be adapted safely".to_string());
        }
        let package = &tokens[package_index];
        let requested_version = package
            .strip_prefix("@openai/codex@")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "Codex package must include a version".to_string())?
            .to_string();
        if requested_version != "latest" {
            validate_version_token(&requested_version)?;
        }
        return Ok(ParsedCodexCommand::Npx {
            package_index,
            tokens,
            requested_version,
        });
    }

    if stem == "codex" {
        return Ok(ParsedCodexCommand::Direct { tokens });
    }

    Err(format!(
        "Custom Codex wrapper `{first}` cannot be adapted safely"
    ))
}

fn split_command_line(command: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut characters = command.chars().peekable();
    while let Some(character) = characters.next() {
        match quote {
            Some(delimiter) if character == delimiter => quote = None,
            Some('"') if character == '\\' && characters.peek() == Some(&'"') => {
                current.push(characters.next().unwrap_or('"'));
            }
            Some(_) => current.push(character),
            None if matches!(character, '\'' | '"') => quote = Some(character),
            None if character.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None if matches!(character, '|' | ';' | '&' | '<' | '>' | '\r' | '\n') => {
                return Err(
                    "Custom Codex wrapper or shell operators cannot be adapted safely".to_string(),
                );
            }
            None => current.push(character),
        }
    }
    if quote.is_some() {
        return Err("Codex command contains an unterminated quote".to_string());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn parse_codex_version(output: &str) -> Result<String, String> {
    let version = output
        .split_whitespace()
        .rev()
        .find(|token| token.chars().any(|character| character.is_ascii_digit()))
        .ok_or_else(|| "Codex version probe returned no version".to_string())?;
    validate_version_token(version)?;
    Ok(version.to_string())
}

fn validate_version_token(version: &str) -> Result<(), String> {
    if version.is_empty()
        || !version.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+')
        })
    {
        return Err(format!(
            "Codex returned an unsafe version token `{version}`"
        ));
    }
    Ok(())
}

fn quote_command_for_shell(tokens: &[String], shell_program: &str) -> String {
    let shell = Path::new(shell_program)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(shell_program)
        .to_ascii_lowercase();
    if shell.contains("powershell") || shell == "pwsh" {
        let command = tokens
            .iter()
            .map(|token| format!("'{}'", token.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(" ");
        return format!("& {command}");
    }
    if shell == "cmd" {
        return tokens
            .iter()
            .map(|token| format!("\"{}\"", token.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(" ");
    }
    tokens
        .iter()
        .map(|token| format!("'{}'", token.replace('\'', "'\"'\"'")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn resolve_executable(program: &str) -> Result<PathBuf, String> {
    let supplied = PathBuf::from(program);
    if supplied.components().count() > 1 || supplied.is_absolute() {
        return supplied
            .canonicalize()
            .map_err(|error| format!("Cannot resolve Codex executable `{program}`: {error}"));
    }

    let path = std::env::var_os("PATH")
        .ok_or_else(|| "PATH is unavailable while resolving Codex".to_string())?;
    let mut names = vec![program.to_string()];
    if cfg!(windows) && Path::new(program).extension().is_none() {
        let extensions =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        names.extend(
            extensions
                .split(';')
                .filter(|extension| !extension.is_empty())
                .map(|extension| format!("{program}{}", extension.to_ascii_lowercase())),
        );
        names.extend(
            extensions
                .split(';')
                .filter(|extension| !extension.is_empty())
                .map(|extension| format!("{program}{}", extension.to_ascii_uppercase())),
        );
    }

    for directory in std::env::split_paths(&path) {
        for name in &names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return candidate.canonicalize().map_err(|error| {
                    format!("Cannot canonicalize `{}`: {error}", candidate.display())
                });
            }
        }
    }
    Err(format!(
        "Codex executable `{program}` was not found on PATH"
    ))
}

fn run_probe(executable: &Path, args: &[String]) -> Result<String, String> {
    let mut command = std::process::Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().map_err(|error| {
        format!(
            "Failed to probe Codex executable `{}`: {error}",
            executable.display()
        )
    })?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || capture_probe_pipe(stdout));
    let stderr_reader = std::thread::spawn(move || capture_probe_pipe(stderr));
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < CODEX_PROBE_TIMEOUT => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!(
                    "Codex capability probe timed out after {} seconds",
                    CODEX_PROBE_TIMEOUT.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("Codex capability probe failed: {error}"));
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    if !status.success() {
        return Err(format!(
            "Codex capability probe exited with {status}: {}",
            truncate_utf8(output.trim(), 2_048)
        ));
    }
    Ok(output)
}

fn capture_probe_pipe<R: std::io::Read>(pipe: Option<R>) -> Vec<u8> {
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let Ok(read) = pipe.read(&mut buffer) else {
            break;
        };
        if read == 0 {
            break;
        }
        let remaining = MAX_PROBE_OUTPUT_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    captured
}

fn string_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

fn string_array(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

fn rpc_id(message: &Value) -> Option<String> {
    let id = message.get("id")?;
    id.as_str()
        .map(str::to_string)
        .or_else(|| id.as_i64().map(|value| value.to_string()))
        .or_else(|| id.as_u64().map(|value| value.to_string()))
}

fn user_message_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|content| match string_field(content, "type") {
            Some("text") => string_field(content, "text").map(str::to_string),
            Some("image") | Some("localImage") => Some("[Image]".to_string()),
            Some("skill") => Some(format!(
                "[Skill: {}]",
                string_field(content, "name").unwrap_or("attached")
            )),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_name(item: &Value, item_type: &str) -> String {
    match item_type {
        "mcpToolCall" => match (string_field(item, "server"), string_field(item, "tool")) {
            (Some(server), Some(tool)) => format!("{server} / {tool}"),
            (_, Some(tool)) => tool.to_string(),
            _ => "MCP tool".to_string(),
        },
        "dynamicToolCall" => string_field(item, "tool")
            .or_else(|| string_field(item, "name"))
            .unwrap_or("Tool")
            .to_string(),
        "webSearch" => "Web search".to_string(),
        _ => item_type.trim_end_matches("ToolCall").to_string(),
    }
}

fn tool_summary(item: &Value) -> String {
    if let Some(error) = item.get("error").and_then(|value| {
        value
            .as_str()
            .or_else(|| value.get("message").and_then(Value::as_str))
    }) {
        return error.to_string();
    }
    if let Some(query) = string_field(item, "query") {
        return query.to_string();
    }
    string_field(item, "status")
        .unwrap_or("Working")
        .to_string()
}

fn tool_state(status: Option<&str>, completed: bool) -> SemanticToolState {
    match status {
        Some("failed" | "declined" | "error") => SemanticToolState::Failed,
        Some("completed" | "success") => SemanticToolState::Completed,
        Some("pending") => SemanticToolState::Pending,
        _ if completed => SemanticToolState::Completed,
        _ => SemanticToolState::Running,
    }
}

fn sanitize_text(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .collect()
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn bounded_identifier(identifier: &str) -> String {
    truncate_utf8(&sanitize_text(identifier), 512).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::presentation::{
        SemanticEventKind, SemanticRetention, SemanticSource, StableSessionKey,
    };
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    fn reducer() -> CodexSemanticReducer {
        CodexSemanticReducer::new(StableSessionKey::from_tab("codex-tab"))
    }

    #[test]
    fn agent_deltas_accumulate_and_completed_item_is_authoritative() {
        let mut reducer = reducer();

        let first = reducer.observe(
            r#"{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"message-1","delta":"Hello "}}"#,
            10,
        );
        let second = reducer.observe(
            r#"{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"thread-1","turnId":"turn-1","itemId":"message-1","delta":"world"}}"#,
            11,
        );
        let completed = reducer.observe(
            r#"{"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","completedAtMs":12,"item":{"id":"message-1","type":"agentMessage","text":"Hello world!"}}}"#,
            12,
        );

        assert!(matches!(
            &first[0].kind,
            SemanticEventKind::AssistantMessage { text, streaming: true, .. } if text == "Hello "
        ));
        assert!(matches!(
            &second[0].kind,
            SemanticEventKind::AssistantMessage { text, streaming: true, .. } if text == "Hello world"
        ));
        assert!(matches!(
            &completed[0].kind,
            SemanticEventKind::AssistantMessage { text, streaming: false, .. } if text == "Hello world!"
        ));
        assert_eq!(first[0].deduplication_key, second[0].deduplication_key);
        assert_eq!(second[0].deduplication_key, completed[0].deduplication_key);
        assert_eq!(completed[0].source, SemanticSource::Codex);
        assert_eq!(completed[0].retention, SemanticRetention::Canonical);
    }

    #[test]
    fn duplicate_or_out_of_order_deltas_do_not_corrupt_authoritative_completion() {
        let mut reducer = reducer();
        let delta = r#"{"method":"item/agentMessage/delta","params":{"threadId":"t","turnId":"u","itemId":"m","delta":"draft"}}"#;

        reducer.observe(delta, 1);
        reducer.observe(delta, 2);
        let completed = reducer.observe(
            r#"{"method":"item/completed","params":{"threadId":"t","turnId":"u","completedAtMs":3,"item":{"id":"m","type":"agentMessage","text":"final"}}}"#,
            3,
        );
        let late = reducer.observe(delta, 4);

        assert!(matches!(
            &completed[0].kind,
            SemanticEventKind::AssistantMessage { text, streaming: false, .. } if text == "final"
        ));
        assert!(
            late.is_empty(),
            "late deltas must not replace a completed item"
        );
    }

    #[test]
    fn command_diff_reasoning_plan_status_and_error_become_native_events() {
        let mut reducer = reducer();

        let command = reducer.observe(
            r#"{"method":"item/completed","params":{"threadId":"t","turnId":"u","completedAtMs":2,"item":{"id":"cmd","type":"commandExecution","command":"cargo test","cwd":"C:/repo","status":"completed","aggregatedOutput":"ok","exitCode":0,"commandActions":[]}}}"#,
            2,
        );
        let diff = reducer.observe(
            r#"{"method":"turn/diff/updated","params":{"threadId":"t","turnId":"u","diff":"--- a/file\n+++ b/file"}}"#,
            3,
        );
        let reasoning = reducer.observe(
            r#"{"method":"item/completed","params":{"threadId":"t","turnId":"u","completedAtMs":4,"item":{"id":"r","type":"reasoning","summary":["Checked the tests"],"content":[]}}}"#,
            4,
        );
        let plan = reducer.observe(
            r#"{"method":"turn/plan/updated","params":{"threadId":"t","turnId":"u","explanation":"Next","plan":[{"step":"Write test","status":"completed"},{"step":"Implement","status":"inProgress"}]}}"#,
            5,
        );
        let status = reducer.observe(
            r#"{"method":"thread/status/changed","params":{"threadId":"t","status":{"type":"active","activeFlags":["waitingOnApproval"]}}}"#,
            6,
        );
        let error = reducer.observe(
            r#"{"method":"error","params":{"threadId":"t","turnId":"u","error":{"message":"network unavailable"},"willRetry":true}}"#,
            7,
        );

        assert!(matches!(
            &command[0].kind,
            SemanticEventKind::Command { text, exit_code: Some(0), .. } if text == "cargo test"
        ));
        assert!(
            matches!(&diff[0].kind, SemanticEventKind::Diff { unified_diff, .. } if unified_diff.contains("+++ b/file"))
        );
        assert!(
            matches!(&reasoning[0].kind, SemanticEventKind::Reasoning { summary, .. } if summary == "Checked the tests")
        );
        assert!(
            matches!(&plan[0].kind, SemanticEventKind::Tool { name, summary, .. } if name == "Plan" && summary.contains("Write test") && summary.contains("Implement"))
        );
        assert!(
            matches!(&status[0].kind, SemanticEventKind::Status { state, detail: Some(detail) } if state == "active" && detail.contains("waitingOnApproval"))
        );
        assert!(
            matches!(&error[0].kind, SemanticEventKind::Error { message } if message.contains("network unavailable") && message.contains("Retrying"))
        );
    }

    #[test]
    fn approval_and_user_input_requests_become_questions() {
        let mut reducer = reducer();

        let approval = reducer.observe(
            r#"{"id":41,"method":"item/commandExecution/requestApproval","params":{"threadId":"t","turnId":"u","itemId":"cmd","startedAtMs":1,"command":"rm tmp.txt","reason":"Delete the temporary file"}}"#,
            1,
        );
        let input = reducer.observe(
            r#"{"id":"request-2","method":"item/tool/requestUserInput","params":{"threadId":"t","turnId":"u","itemId":"ask","questions":[{"id":"scope","header":"Scope","question":"Which scope?","options":[{"label":"Current","description":"Current project"},{"label":"All","description":"All projects"}]}]}}"#,
            2,
        );

        assert!(
            matches!(&approval[0].kind, SemanticEventKind::Question { prompt, choices, .. } if prompt.contains("rm tmp.txt") && choices.as_slice() == ["Approve", "Decline"])
        );
        assert!(
            matches!(&input[0].kind, SemanticEventKind::Question { question_id, prompt, choices } if question_id.contains("scope") && prompt == "Which scope?" && choices.as_slice() == ["Current", "All"])
        );
    }

    #[test]
    fn malformed_unknown_and_response_messages_are_ignored_without_panicking() {
        let mut reducer = reducer();
        assert!(reducer.observe("not json", 1).is_empty());
        assert!(reducer
            .observe(
                r#"{"method":"future/protocol/message","params":{"secret":"untouched"}}"#,
                2
            )
            .is_empty());
        assert!(reducer
            .observe(r#"{"id":1,"result":{"thread":{"id":"t"}}}"#, 3)
            .is_empty());
    }

    #[test]
    fn reducer_captures_latest_protocol_identifiers_without_projecting_responses() {
        let mut reducer = reducer();
        assert!(reducer
            .observe(
                r#"{"id":1,"result":{"thread":{"id":"thread-from-response"}}}"#,
                1,
            )
            .is_empty());
        reducer.observe(
            r#"{"method":"item/started","params":{"threadId":"thread-2","turnId":"turn-2","startedAtMs":2,"item":{"id":"item-2","type":"commandExecution","command":"pwd","status":"inProgress","commandActions":[],"cwd":"C:/repo"}}}"#,
            2,
        );

        assert_eq!(
            reducer.protocol_state(),
            CodexProtocolState {
                thread_id: Some("thread-2".to_string()),
                turn_id: Some("turn-2".to_string()),
                item_id: Some("item-2".to_string()),
            }
        );
    }

    #[test]
    fn observer_memory_and_visible_text_are_bounded() {
        let mut reducer = CodexSemanticReducer::with_limits(
            StableSessionKey::from_tab("codex-tab"),
            CodexReducerLimits {
                active_items: 2,
                item_bytes: 12,
                total_bytes: 20,
            },
        );

        for index in 0..8 {
            reducer.observe(
                &format!(
                    r#"{{"method":"item/agentMessage/delta","params":{{"threadId":"t","turnId":"u","itemId":"m{index}","delta":"abcdefghijklmno"}}}}"#
                ),
                index,
            );
        }

        let usage = reducer.memory_usage();
        assert!(usage.active_items <= 2);
        assert!(usage.text_bytes <= 20);
        let current = reducer.observe(
            r#"{"method":"item/agentMessage/delta","params":{"threadId":"t","turnId":"u","itemId":"latest","delta":"abcdefghijklmnop"}}"#,
            20,
        );
        assert!(
            matches!(&current[0].kind, SemanticEventKind::AssistantMessage { text, .. } if text.len() <= 12)
        );
    }

    #[test]
    fn transparent_forwarding_preserves_every_byte_when_observer_is_saturated() {
        let (observer, receiver) = semantic_observer_channel(1);
        let first = br#"{"method":"future/event","params":{"spacing":"  exact  "}}"#;
        let second = b"not-json-at-all\r\n\0";

        assert_eq!(forward_server_frame(first, 1, &observer), first);
        assert_eq!(forward_server_frame(second, 2, &observer), second);
        assert_eq!(receiver.try_recv().unwrap().bytes, first);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn observer_byte_budget_drops_oversize_frames_without_affecting_forwarding() {
        let (observer, receiver) = semantic_observer_channel_with_limits(4, 8);
        let frame = b"123456789";
        assert_eq!(forward_server_frame(frame, 1, &observer), frame);
        assert!(receiver.try_recv().is_err());
        assert_eq!(observer.queued_bytes(), 0);
    }

    #[tokio::test]
    async fn jsonl_reader_rejects_oversize_frame_without_unbounded_growth() {
        let (mut writer, reader) = tokio::io::duplex(32);
        writer.write_all(b"123456789\n").await.unwrap();
        let mut reader = BufReader::new(reader);
        let mut frame = Vec::new();
        let error = read_jsonl_frame(&mut reader, &mut frame, 8)
            .await
            .unwrap_err();
        assert!(error.contains("exceeded"));
        assert!(frame.len() <= 8);
    }

    #[tokio::test]
    async fn unknown_json_rpc_round_trips_unchanged_through_loopback_proxy() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let (bridge_stdio, fake_server_stdio) = tokio::io::duplex(64 * 1024);
        let (observer, _receiver) = semantic_observer_channel(4);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let proxy = tokio::spawn(serve_one_loopback_client(
            listener,
            bridge_stdio,
            observer,
            shutdown_rx,
        ));

        let (mut tui, _) = connect_async(&endpoint).await.unwrap();
        let (fake_read, mut fake_write) = tokio::io::split(fake_server_stdio);
        let mut fake_read = BufReader::new(fake_read);

        let server_raw =
            r#"{"method":"future/event", "params": {"opaque":true, "spacing":"  exact  "}}"#;
        fake_write.write_all(server_raw.as_bytes()).await.unwrap();
        fake_write.write_all(b"\n").await.unwrap();
        fake_write.flush().await.unwrap();
        assert_eq!(
            tui.next().await.unwrap().unwrap(),
            Message::Text(server_raw.to_string().into())
        );

        let initialize = r#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"codex-tui","version":"0.144.3"}}}"#;
        tui.send(Message::Text(initialize.to_string().into()))
            .await
            .unwrap();
        let mut forwarded = Vec::new();
        fake_read.read_until(b'\n', &mut forwarded).await.unwrap();
        assert_eq!(forwarded, format!("{initialize}\n").as_bytes());

        let tui_raw = r#"{"id":91,"method":"future/request","params":{"unknown":[1,2,3]}}"#;
        tui.send(Message::Text(tui_raw.to_string().into()))
            .await
            .unwrap();
        forwarded.clear();
        fake_read.read_until(b'\n', &mut forwarded).await.unwrap();
        assert_eq!(forwarded, format!("{tui_raw}\n").as_bytes());

        let _ = shutdown_tx.send(());
        proxy.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn proxy_rejects_non_loopback_peer_before_websocket_upgrade() {
        assert!(!peer_is_allowed("192.0.2.10:1234".parse().unwrap()));
        assert!(peer_is_allowed("127.0.0.1:1234".parse().unwrap()));
        assert!(peer_is_allowed("[::1]:1234".parse().unwrap()));
    }

    #[test]
    fn latest_npx_command_is_resolved_once_then_both_processes_are_pinned() {
        let mut calls = Vec::<Vec<String>>::new();
        let prepared = prepare_codex_adapter_with(
            "npx -y @openai/codex@latest --dangerously-bypass-approvals-and-sandbox",
            |_| Ok(std::path::PathBuf::from("C:/Program Files/nodejs/npx.cmd")),
            |_, args| {
                calls.push(args.to_vec());
                if args.last().is_some_and(|arg| arg == "--version") {
                    return Ok("codex-cli 0.144.3\n".to_string());
                }
                if args.last().is_some_and(|arg| arg == "--help")
                    && args.iter().any(|arg| arg == "app-server")
                {
                    return Ok("Usage: codex app-server --listen <URI>".to_string());
                }
                if args.last().is_some_and(|arg| arg == "--help") {
                    return Ok("Usage: codex [OPTIONS]\n  --remote <WS_URL>".to_string());
                }
                Err("unexpected probe".to_string())
            },
        )
        .unwrap();

        assert_eq!(prepared.version(), "0.144.3");
        assert_eq!(
            calls
                .iter()
                .filter(|args| args.iter().any(|arg| arg.contains("@latest")))
                .count(),
            1,
            "@latest must only be resolved by the first version probe"
        );
        assert!(prepared
            .sidecar_args()
            .starts_with(&["-y".to_string(), "@openai/codex@0.144.3".to_string()]));
        assert!(prepared.sidecar_args().ends_with(&[
            "app-server".to_string(),
            "--listen".to_string(),
            "stdio://".to_string()
        ]));
        let command = prepared.tui_command("ws://127.0.0.1:49152", "powershell.exe");
        assert!(command.starts_with("& 'C:/Program Files/nodejs/npx.cmd'"));
        assert!(command.contains("'@openai/codex@0.144.3'"));
        assert!(command.contains("'--remote' 'ws://127.0.0.1:49152'"));
        assert!(!command.contains("@latest"));
    }

    #[test]
    fn unsupported_wrappers_and_missing_capabilities_fail_before_launch() {
        let wrapper = prepare_codex_adapter_with(
            "cmd /c codex --dangerously-bypass-approvals-and-sandbox",
            |_| panic!("wrapper must fail before executable lookup"),
            |_, _| panic!("wrapper must fail before probing"),
        );
        assert!(wrapper.unwrap_err().contains("wrapper"));

        let missing_remote = prepare_codex_adapter_with(
            "codex --dangerously-bypass-approvals-and-sandbox",
            |_| Ok(std::path::PathBuf::from("C:/tools/codex.exe")),
            |_, args| {
                if args.last().is_some_and(|arg| arg == "--version") {
                    Ok("codex-cli 0.144.3".to_string())
                } else if args.iter().any(|arg| arg == "app-server") {
                    Ok("--listen".to_string())
                } else {
                    Ok("no remote option here".to_string())
                }
            },
        );
        assert!(missing_remote.unwrap_err().contains("--remote"));
    }

    #[test]
    fn direct_executable_uses_the_same_resolved_path_for_sidecar_and_tui() {
        let prepared = prepare_codex_adapter_with(
            "codex --full-auto",
            |_| Ok(std::path::PathBuf::from("C:/exact/codex.exe")),
            |_, args| {
                if args.last().is_some_and(|arg| arg == "--version") {
                    Ok("codex-cli 0.144.3".to_string())
                } else if args.iter().any(|arg| arg == "app-server") {
                    Ok("--listen".to_string())
                } else {
                    Ok("--remote".to_string())
                }
            },
        )
        .unwrap();

        assert_eq!(
            prepared.executable(),
            std::path::Path::new("C:/exact/codex.exe")
        );
        assert_eq!(
            prepared.tui_command("ws://127.0.0.1:1", "bash"),
            "'C:/exact/codex.exe' '--full-auto' '--remote' 'ws://127.0.0.1:1'"
        );
    }

    #[tokio::test]
    async fn bridge_handle_owns_sidecar_and_shuts_it_down_cleanly() {
        #[cfg(windows)]
        let prepared = PreparedCodexAdapter {
            executable: resolve_executable("powershell.exe").unwrap(),
            sidecar_prefix_args: vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                "while (($line = [Console]::In.ReadLine()) -ne $null) { [Console]::Out.WriteLine($line); [Console]::Out.Flush() }".to_string(),
            ],
            tui_args: Vec::new(),
            version: "test".to_string(),
        };
        #[cfg(not(windows))]
        let prepared = PreparedCodexAdapter {
            executable: std::path::PathBuf::from("/bin/sh"),
            sidecar_prefix_args: vec!["-c".to_string(), "cat".to_string()],
            tui_args: Vec::new(),
            version: "test".to_string(),
        };

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let mut bridge = CodexBridgeHandle::start(
            prepared,
            std::env::current_dir().unwrap(),
            StableSessionKey::from_tab("codex-tab"),
            move |event| captured.lock().unwrap().push(event),
            |_| {},
        )
        .unwrap();
        let (mut tui, _) = connect_async(bridge.endpoint()).await.unwrap();
        let raw = r#"{"method":"thread/status/changed","params":{"threadId":"t","status":{"type":"idle"}}}"#;
        tui.send(Message::Text(raw.to_string().into()))
            .await
            .unwrap();
        assert_eq!(
            tui.next().await.unwrap().unwrap(),
            Message::Text(raw.to_string().into())
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !events.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        bridge.shutdown();
    }

    #[test]
    fn sidecar_spawn_failure_returns_without_a_live_bridge() {
        let prepared = PreparedCodexAdapter {
            executable: std::env::temp_dir().join("devmanager-missing-codex-sidecar.exe"),
            sidecar_prefix_args: Vec::new(),
            tui_args: Vec::new(),
            version: "test".to_string(),
        };
        let result = CodexBridgeHandle::start(
            prepared,
            std::env::current_dir().unwrap(),
            StableSessionKey::from_tab("codex-tab"),
            |_| {},
            |_| {},
        );
        let error = result.unwrap_err();
        assert!(
            error.contains("start Codex app-server"),
            "unexpected bridge error: {error}"
        );
    }
}
