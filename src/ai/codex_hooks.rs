//! Codex hooks tap: relays Codex lifecycle/tool/approval hook payloads into
//! DevManager's semantic journal without altering how the Codex TUI runs.
//! Mirrors the Claude hooks relay (`claude_hooks.rs`).

use crate::ai::claude_hooks::is_valid_loopback_relay_url_for;
use crate::remote::presentation::{
    SemanticEventDraft, SemanticEventKind, SemanticRetention, SemanticSource, SemanticToolState,
    StableSessionKey,
};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

pub const CODEX_HOOK_RELAY_PATH: &str = "/internal/codex-hook";
pub const MAX_CODEX_HOOK_BODY_BYTES: usize = 256 * 1024;
const MAX_CODEX_HOOK_TEXT_BYTES: usize = 64 * 1024;
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

fn bounded_hook_text(value: &str) -> String {
    if value.len() <= MAX_CODEX_HOOK_TEXT_BYTES {
        return value.to_string();
    }
    let budget = MAX_CODEX_HOOK_TEXT_BYTES.saturating_sub(TRUNCATION_SUFFIX.len());
    format!("{}{TRUNCATION_SUFFIX}", truncate_utf8(value, budget))
}

fn bounded_identifier(value: &str) -> String {
    value.chars().take(256).collect()
}

fn string_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

/// Session facts delivered by the SessionStart hook; binds a PTY session to
/// the rollout transcript that the tailer follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionBinding {
    pub session_id: String,
    pub transcript_path: Option<PathBuf>,
    pub cwd: PathBuf,
}

#[derive(Debug, Default)]
pub struct CodexHookReduction {
    pub drafts: Vec<SemanticEventDraft>,
    pub session_binding: Option<CodexSessionBinding>,
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

/// Tolerant projection of Codex hook stdin payloads. Codex remains the schema
/// authority: unknown events and missing fields must never fail the session.
pub struct CodexHookReducer {
    stable_session_key: StableSessionKey,
    tool_states: HashMap<String, SemanticToolState>,
}

impl CodexHookReducer {
    pub fn new(stable_session_key: StableSessionKey) -> Self {
        Self {
            stable_session_key,
            tool_states: HashMap::new(),
        }
    }

    pub fn apply_json(&mut self, payload: &Value, occurred_at_epoch_ms: u64) -> CodexHookReduction {
        let Some(event_name) = string_field(payload, "hook_event_name") else {
            return CodexHookReduction::default();
        };
        let session_id = string_field(payload, "session_id")
            .map(bounded_identifier)
            .unwrap_or_else(|| "unknown".to_string());

        match event_name {
            "SessionStart" => {
                let binding = CodexSessionBinding {
                    session_id: session_id.clone(),
                    transcript_path: string_field(payload, "transcript_path")
                        .filter(|path| !path.is_empty())
                        .map(PathBuf::from),
                    cwd: PathBuf::from(string_field(payload, "cwd").unwrap_or_default()),
                };
                CodexHookReduction {
                    drafts: vec![self.event(
                        occurred_at_epoch_ms,
                        SemanticEventKind::Status {
                            state: "ready".to_string(),
                            detail: None,
                        },
                        SemanticRetention::Canonical,
                        format!("codex-hook:session:{session_id}"),
                    )],
                    session_binding: Some(binding),
                }
            }
            "UserPromptSubmit" => {
                let Some(prompt) = string_field(payload, "prompt").filter(|text| !text.is_empty())
                else {
                    return CodexHookReduction::default();
                };
                CodexHookReduction {
                    drafts: vec![self.event(
                        occurred_at_epoch_ms,
                        SemanticEventKind::UserMessage {
                            text: bounded_hook_text(prompt),
                        },
                        SemanticRetention::Canonical,
                        format!("codex-hook:user:{session_id}:{occurred_at_epoch_ms}"),
                    )],
                    session_binding: None,
                }
            }
            "PreToolUse" => self.tool_reduction(
                payload,
                occurred_at_epoch_ms,
                SemanticToolState::Running,
            ),
            "PostToolUse" => self.tool_reduction(
                payload,
                occurred_at_epoch_ms,
                SemanticToolState::Completed,
            ),
            "PermissionRequest" => {
                let tool_name = string_field(payload, "tool_name").unwrap_or("a tool");
                let tool_use_id = string_field(payload, "tool_use_id")
                    .map(bounded_identifier)
                    .unwrap_or_else(|| "unknown".to_string());
                let summary = tool_input_summary(payload);
                let question_id = format!("codex-hook:{session_id}:{tool_use_id}");
                let prompt = if summary.is_empty() {
                    format!("Codex requests permission to run {tool_name}")
                } else {
                    format!("Codex requests permission to run {tool_name}\n\n{summary}")
                };
                CodexHookReduction {
                    drafts: vec![self.event(
                        occurred_at_epoch_ms,
                        SemanticEventKind::Question {
                            question_id: question_id.clone(),
                            prompt,
                            choices: vec!["Approve".to_string(), "Decline".to_string()],
                        },
                        SemanticRetention::Canonical,
                        format!("codex-hook:question:{question_id}"),
                    )],
                    session_binding: None,
                }
            }
            "Stop" => CodexHookReduction {
                drafts: vec![self.event(
                    occurred_at_epoch_ms,
                    SemanticEventKind::Status {
                        state: "idle".to_string(),
                        detail: None,
                    },
                    SemanticRetention::Canonical,
                    format!("codex-hook:turn-status:{session_id}"),
                )],
                session_binding: None,
            },
            _ => CodexHookReduction::default(),
        }
    }

    fn tool_reduction(
        &mut self,
        payload: &Value,
        occurred_at_epoch_ms: u64,
        requested: SemanticToolState,
    ) -> CodexHookReduction {
        let Some(tool_use_id) = string_field(payload, "tool_use_id").map(bounded_identifier)
        else {
            return CodexHookReduction::default();
        };
        let tool_name = string_field(payload, "tool_name").unwrap_or("Tool");
        let current = self
            .tool_states
            .get(&tool_use_id)
            .copied()
            .unwrap_or(SemanticToolState::Pending);
        if !should_advance_tool_state(current, requested) {
            return CodexHookReduction::default();
        }
        self.tool_states.insert(tool_use_id.clone(), requested);
        // Bound the number of remembered tools alongside their text.
        if self.tool_states.len() > 256 {
            self.tool_states.clear();
            self.tool_states.insert(tool_use_id.clone(), requested);
        }
        CodexHookReduction {
            drafts: vec![self.event(
                occurred_at_epoch_ms,
                SemanticEventKind::Tool {
                    tool_id: tool_use_id.clone(),
                    name: tool_name.to_string(),
                    state: requested,
                    summary: tool_input_summary(payload),
                },
                SemanticRetention::Canonical,
                format!("codex-hook:tool:{tool_use_id}"),
            )],
            session_binding: None,
        }
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

const CODEX_NONCE_BYTES: usize = 16;
const MAX_CODEX_REGISTRATIONS: usize = 128;

fn random_codex_nonce() -> Result<String, String> {
    let mut bytes = [0_u8; CODEX_NONCE_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("generate Codex hook nonce: {error}"))?;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    Ok(encoded)
}

fn unix_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexHookRegistration {
    pub nonce: String,
    pub stable_session_key: StableSessionKey,
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub enum CodexRegistryEvent {
    Semantic(SemanticEventDraft),
    SessionStarted(CodexSessionBinding),
}

pub type CodexRegistryEventHandler =
    std::sync::Arc<dyn Fn(CodexHookRegistration, CodexRegistryEvent) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexRelayIngestStatus {
    Accepted,
    Rejected,
    BodyTooLarge,
    Malformed,
}

struct RegisteredCodexSession {
    stable_session_key: StableSessionKey,
    generation: u64,
    reducer: CodexHookReducer,
}

struct CodexRegistryState {
    registrations: HashMap<String, RegisteredCodexSession>,
    order: std::collections::VecDeque<String>,
    next_generation: u64,
    latest_generation_by_key: HashMap<StableSessionKey, u64>,
}

/// Nonce- and generation-fenced ingest for Codex hook relay payloads.
/// Superseded registrations (an older launch of the same session key) are
/// rejected so a stale relaunch can never publish into the newer session.
///
/// The registered event handler is invoked while a publication read-guard is
/// held: it must not call back into the registry.
pub struct CodexHookRegistry {
    publication_gate: std::sync::RwLock<()>,
    state: std::sync::Mutex<CodexRegistryState>,
    event_handler: std::sync::RwLock<Option<CodexRegistryEventHandler>>,
}

impl Default for CodexHookRegistry {
    fn default() -> Self {
        Self {
            publication_gate: std::sync::RwLock::new(()),
            state: std::sync::Mutex::new(CodexRegistryState {
                registrations: HashMap::new(),
                order: std::collections::VecDeque::new(),
                next_generation: 0,
                latest_generation_by_key: HashMap::new(),
            }),
            event_handler: std::sync::RwLock::new(None),
        }
    }
}

impl CodexHookRegistry {
    pub fn set_event_handler(&self, handler: Option<CodexRegistryEventHandler>) {
        if let Ok(mut slot) = self.event_handler.write() {
            *slot = handler;
        }
    }

    pub fn register(
        &self,
        stable_session_key: StableSessionKey,
    ) -> Result<CodexHookRegistration, String> {
        let _publication = self
            .publication_gate
            .write()
            .map_err(|_| "Codex hook publication gate is poisoned".to_string())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Codex hook registry lock is poisoned".to_string())?;
        while state.registrations.len() >= MAX_CODEX_REGISTRATIONS {
            let Some(oldest) = state.order.pop_front() else {
                break;
            };
            state.registrations.remove(&oldest);
        }
        let nonce = loop {
            let candidate = random_codex_nonce()?;
            if !state.registrations.contains_key(&candidate) {
                break candidate;
            }
        };
        let generation = state
            .next_generation
            .checked_add(1)
            .ok_or_else(|| "Codex hook registration generation exhausted".to_string())?;
        state.next_generation = generation;
        state
            .latest_generation_by_key
            .insert(stable_session_key.clone(), generation);
        state.order.push_back(nonce.clone());
        state.registrations.insert(
            nonce.clone(),
            RegisteredCodexSession {
                stable_session_key: stable_session_key.clone(),
                generation,
                reducer: CodexHookReducer::new(stable_session_key.clone()),
            },
        );
        Ok(CodexHookRegistration {
            nonce,
            stable_session_key,
            generation,
        })
    }

    pub fn unregister(&self, nonce: &str) -> Option<StableSessionKey> {
        let _publication = self.publication_gate.write().ok()?;
        let mut state = self.state.lock().ok()?;
        let removed = state.registrations.remove(nonce)?;
        state.order.retain(|candidate| candidate != nonce);
        if state
            .latest_generation_by_key
            .get(&removed.stable_session_key)
            == Some(&removed.generation)
        {
            state
                .latest_generation_by_key
                .remove(&removed.stable_session_key);
        }
        Some(removed.stable_session_key)
    }

    pub fn ingest(
        &self,
        peer: std::net::SocketAddr,
        nonce: &str,
        body: &[u8],
        occurred_at_epoch_ms: u64,
    ) -> CodexRelayIngestStatus {
        if !peer.ip().is_loopback() {
            return CodexRelayIngestStatus::Rejected;
        }
        if body.len() > MAX_CODEX_HOOK_BODY_BYTES {
            return CodexRelayIngestStatus::BodyTooLarge;
        }
        let Ok(payload) = serde_json::from_slice::<Value>(body) else {
            return CodexRelayIngestStatus::Malformed;
        };
        let Ok(_publication) = self.publication_gate.read() else {
            return CodexRelayIngestStatus::Rejected;
        };
        let (registration, reduction) = {
            let Ok(mut state) = self.state.lock() else {
                return CodexRelayIngestStatus::Rejected;
            };
            let latest = {
                let Some(session) = state.registrations.get(nonce) else {
                    return CodexRelayIngestStatus::Rejected;
                };
                state
                    .latest_generation_by_key
                    .get(&session.stable_session_key)
                    == Some(&session.generation)
            };
            if !latest {
                state.registrations.remove(nonce);
                state.order.retain(|candidate| candidate != nonce);
                return CodexRelayIngestStatus::Rejected;
            }
            let Some(session) = state.registrations.get_mut(nonce) else {
                return CodexRelayIngestStatus::Rejected;
            };
            let registration = CodexHookRegistration {
                nonce: nonce.to_string(),
                stable_session_key: session.stable_session_key.clone(),
                generation: session.generation,
            };
            (
                registration,
                session.reducer.apply_json(&payload, occurred_at_epoch_ms),
            )
        };
        let handler = self
            .event_handler
            .read()
            .ok()
            .and_then(|slot| slot.clone());
        if let Some(handler) = handler {
            if let Some(binding) = reduction.session_binding {
                handler(
                    registration.clone(),
                    CodexRegistryEvent::SessionStarted(binding),
                );
            }
            for draft in reduction.drafts {
                handler(registration.clone(), CodexRegistryEvent::Semantic(draft));
            }
        }
        CodexRelayIngestStatus::Accepted
    }
}

/// Loopback HTTP listener for `codex-hook-relay` POSTs. One per process,
/// started lazily by the process manager; sessions are multiplexed by nonce.
pub struct CodexHookRelayListener {
    endpoint: String,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    server_thread: Option<std::thread::JoinHandle<()>>,
}

impl CodexHookRelayListener {
    pub fn start(registry: std::sync::Arc<CodexHookRegistry>) -> Result<Self, String> {
        use axum::routing::post;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| format!("bind Codex hook relay: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("configure Codex hook relay: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read Codex hook relay address: {error}"))?;
        let endpoint = format!(
            "http://127.0.0.1:{}{CODEX_HOOK_RELAY_PATH}",
            address.port()
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("start Codex hook relay runtime: {error}"))?;
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server_thread = std::thread::Builder::new()
            .name("codex-hook-relay".to_string())
            .spawn(move || {
                runtime.block_on(async move {
                    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
                        return;
                    };
                    let app = axum::Router::new()
                        .route(CODEX_HOOK_RELAY_PATH, post(handle_codex_hook))
                        .layer(axum::extract::DefaultBodyLimit::max(
                            MAX_CODEX_HOOK_BODY_BYTES,
                        ))
                        .with_state(registry);
                    let _ = axum::serve(
                        listener,
                        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                    )
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await;
                });
            })
            .map_err(|error| format!("spawn Codex hook relay: {error}"))?;
        Ok(Self {
            endpoint,
            shutdown_tx: Some(shutdown_tx),
            server_thread: Some(server_thread),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl Drop for CodexHookRelayListener {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(thread) = self.server_thread.take() {
            let _ = thread.join();
        }
    }
}

async fn handle_codex_hook(
    axum::extract::State(registry): axum::extract::State<std::sync::Arc<CodexHookRegistry>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::http::StatusCode {
    let Some(nonce) = headers
        .get("x-devmanager-codex-nonce")
        .and_then(|value| value.to_str().ok())
    else {
        return axum::http::StatusCode::UNAUTHORIZED;
    };
    match registry.ingest(peer, nonce, &body, unix_epoch_ms()) {
        CodexRelayIngestStatus::Accepted | CodexRelayIngestStatus::Malformed => {
            axum::http::StatusCode::NO_CONTENT
        }
        CodexRelayIngestStatus::Rejected => axum::http::StatusCode::UNAUTHORIZED,
        CodexRelayIngestStatus::BodyTooLarge => axum::http::StatusCode::PAYLOAD_TOO_LARGE,
    }
}

pub const CODEX_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
];
const CODEX_HOOK_TRUST_FLAG: &str = "--dangerously-bypass-hook-trust";

/// Renders a TOML basic string: wraps in double quotes, escapes \ and ",
/// and encodes control characters so the value survives `-c key=value`.
pub fn toml_basic_string(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() + 2);
    encoded.push('"');
    for character in value.chars() {
        match character {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            '\u{0008}' => encoded.push_str("\\b"),
            '\t' => encoded.push_str("\\t"),
            '\n' => encoded.push_str("\\n"),
            '\u{000C}' => encoded.push_str("\\f"),
            '\r' => encoded.push_str("\\r"),
            character if (character as u32) < 0x20 || character == '\u{7f}' => {
                encoded.push_str(&format!("\\u{:04X}", character as u32));
            }
            character => encoded.push(character),
        }
    }
    encoded.push('"');
    encoded
}

/// Capability probe: runs the user's configured command with `--help`
/// appended (clap gives `--help` precedence over other flags, so this
/// exercises the real package an npx wrapper resolves) and requires the
/// hook-trust bypass flag to be advertised. `Err` means: launch the
/// configured command verbatim and report the adapter Degraded — an older
/// Codex would hard-fail on the unknown flag if we injected it anyway.
pub fn codex_supports_hooks(startup_command: &str) -> Result<(), String> {
    let tokens = crate::ai::codex_bridge::split_command_line(startup_command)?;
    let Some(first) = tokens.first() else {
        return Err("Codex command is empty".to_string());
    };
    let executable = crate::ai::codex_bridge::resolve_executable(first)?;
    let mut probe_args = tokens[1..].to_vec();
    probe_args.push("--help".to_string());
    let help = crate::ai::codex_bridge::run_probe(&executable, &probe_args)?;
    if !crate::ai::codex_bridge::help_advertises_flag(&help, CODEX_HOOK_TRUST_FLAG) {
        return Err(format!(
            "Codex does not advertise the required {CODEX_HOOK_TRUST_FLAG} capability"
        ));
    }
    Ok(())
}

/// The full command line for the PTY: the user's command exactly as written,
/// plus one `-c hooks.<Event>=[...]` override per registered event and the
/// hook-trust bypass flag. No path resolution, no version pinning.
pub fn build_codex_hooks_command(
    startup_command: &str,
    shell_program: &str,
    devmanager_executable: &std::path::Path,
    endpoint: &str,
    nonce: &str,
    config: &[crate::ai::codex_bridge::CodexConfigOverride],
) -> Result<String, String> {
    if !is_valid_loopback_relay_url_for(endpoint, CODEX_HOOK_RELAY_PATH) {
        return Err("Codex hook relay endpoint is not an exact loopback URL".to_string());
    }
    if nonce.is_empty() || !nonce.chars().all(|character| character.is_ascii_hexdigit()) {
        return Err("Codex hook relay nonce must be non-empty hex".to_string());
    }
    let mut tokens = crate::ai::codex_bridge::split_command_line(startup_command)?;
    if tokens.is_empty() {
        return Err("Codex command is empty".to_string());
    }
    // Codex runs hook commands through a shell; double quotes around the
    // executable path are safe on cmd, PowerShell, and sh alike.
    let relay_command = format!(
        "\"{}\" codex-hook-relay --url {endpoint} --nonce {nonce}",
        devmanager_executable.to_string_lossy()
    );
    for override_value in config {
        tokens.push("--config".to_string());
        tokens.push(override_value.argument());
    }
    for event in CODEX_HOOK_EVENTS {
        let override_value = format!(
            "hooks.{event}=[{{hooks=[{{type=\"command\",command={},async=true}}]}}]",
            toml_basic_string(&relay_command)
        );
        tokens.push("-c".to_string());
        tokens.push(override_value);
    }
    tokens.push(CODEX_HOOK_TRUST_FLAG.to_string());
    Ok(crate::ai::codex_bridge::quote_command_for_shell(
        &tokens,
        shell_program,
    ))
}

fn tool_input_summary(payload: &Value) -> String {
    let Some(tool_input) = payload.get("tool_input") else {
        return String::new();
    };
    let rendered = match tool_input.get("command").and_then(Value::as_str) {
        Some(command) => command.to_string(),
        None => match tool_input {
            Value::String(text) => text.clone(),
            Value::Null => String::new(),
            other => serde_json::to_string(other).unwrap_or_default(),
        },
    };
    bounded_hook_text(&rendered)
}

pub fn run_codex_hook_relay(endpoint: &str, nonce: &str, body: &[u8]) -> ExitCode {
    if body.len() > MAX_CODEX_HOOK_BODY_BYTES
        || !is_valid_loopback_relay_url_for(endpoint, CODEX_HOOK_RELAY_PATH)
    {
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
        .header("x-devmanager-codex-nonce", nonce)
        .send(body);
    ExitCode::SUCCESS
}

pub fn run_codex_hook_relay_subcommand<R: Read>(args: &[String], reader: R) -> Option<ExitCode> {
    if args.first().map(String::as_str) != Some("codex-hook-relay") {
        return None;
    }
    let [_, url_flag, endpoint, nonce_flag, nonce] = args else {
        return Some(ExitCode::SUCCESS);
    };
    if url_flag != "--url" || nonce_flag != "--nonce" {
        return Some(ExitCode::SUCCESS);
    }
    let mut body = Vec::new();
    let mut limited = reader.take((MAX_CODEX_HOOK_BODY_BYTES + 1) as u64);
    if limited.read_to_end(&mut body).is_err() || body.len() > MAX_CODEX_HOOK_BODY_BYTES {
        return Some(ExitCode::SUCCESS);
    }
    Some(run_codex_hook_relay(endpoint, nonce, &body))
}

#[cfg(test)]
mod reducer_tests {
    use super::*;

    fn test_reducer() -> CodexHookReducer {
        CodexHookReducer::new(StableSessionKey::from_tab("tab-1"))
    }

    #[test]
    fn session_start_produces_binding_and_ready_status() {
        let mut reducer = test_reducer();
        let payload = serde_json::json!({
            "session_id": "019f-abc", "cwd": "C:\\proj",
            "transcript_path": "C:\\Users\\u\\.codex\\sessions\\2026\\07\\17\\rollout-x.jsonl",
            "hook_event_name": "SessionStart", "model": "gpt-5",
            "permission_mode": "danger-full-access"
        });
        let out = reducer.apply_json(&payload, 1);
        let binding = out.session_binding.unwrap();
        assert_eq!(binding.session_id, "019f-abc");
        assert!(binding.transcript_path.unwrap().ends_with("rollout-x.jsonl"));
        assert!(matches!(
            out.drafts[0].kind,
            SemanticEventKind::Status { ref state, .. } if state == "ready"
        ));
    }

    #[test]
    fn permission_request_produces_question() {
        let mut reducer = test_reducer();
        let payload = serde_json::json!({
            "session_id": "019f-abc", "cwd": "C:\\proj", "transcript_path": null,
            "hook_event_name": "PermissionRequest", "model": "gpt-5",
            "permission_mode": "on-request",
            "tool_name": "shell", "tool_input": {"command": "rm -rf build"},
            "tool_use_id": "call_1"
        });
        let out = reducer.apply_json(&payload, 1);
        match &out.drafts[0].kind {
            SemanticEventKind::Question {
                question_id,
                prompt,
                choices,
            } => {
                assert_eq!(question_id, "codex-hook:019f-abc:call_1");
                assert!(prompt.contains("shell"));
                assert!(prompt.contains("rm -rf build"));
                assert_eq!(choices, &vec!["Approve".to_string(), "Decline".to_string()]);
            }
            other => panic!("expected question, got {other:?}"),
        }
    }

    #[test]
    fn pre_tool_use_produces_running_tool() {
        let mut reducer = test_reducer();
        let payload = serde_json::json!({
            "session_id": "s", "hook_event_name": "PreToolUse",
            "tool_name": "shell", "tool_input": {"command": "cargo build"},
            "tool_use_id": "call_2"
        });
        let out = reducer.apply_json(&payload, 2);
        match &out.drafts[0].kind {
            SemanticEventKind::Tool {
                tool_id,
                name,
                state,
                summary,
            } => {
                assert_eq!(tool_id, "call_2");
                assert_eq!(name, "shell");
                assert_eq!(*state, SemanticToolState::Running);
                assert_eq!(summary, "cargo build");
            }
            other => panic!("expected tool, got {other:?}"),
        }
    }

    #[test]
    fn post_tool_use_completes_tool_and_stale_pre_is_dropped() {
        let mut reducer = test_reducer();
        let pre = serde_json::json!({
            "session_id": "s", "hook_event_name": "PreToolUse",
            "tool_name": "shell", "tool_input": {}, "tool_use_id": "call_3"
        });
        let post = serde_json::json!({
            "session_id": "s", "hook_event_name": "PostToolUse",
            "tool_name": "shell", "tool_input": {}, "tool_use_id": "call_3"
        });
        assert_eq!(reducer.apply_json(&pre, 1).drafts.len(), 1);
        let out = reducer.apply_json(&post, 2);
        assert!(matches!(
            &out.drafts[0].kind,
            SemanticEventKind::Tool { state, .. } if *state == SemanticToolState::Completed
        ));
        // A late duplicate PreToolUse must not regress the completed state.
        assert!(reducer.apply_json(&pre, 3).drafts.is_empty());
    }

    #[test]
    fn user_prompt_submit_produces_user_message() {
        let mut reducer = test_reducer();
        let payload = serde_json::json!({
            "session_id": "s", "hook_event_name": "UserPromptSubmit",
            "prompt": "fix the bug"
        });
        let out = reducer.apply_json(&payload, 1);
        assert!(matches!(
            &out.drafts[0].kind,
            SemanticEventKind::UserMessage { text } if text == "fix the bug"
        ));
    }

    #[test]
    fn stop_produces_idle_status() {
        let mut reducer = test_reducer();
        let payload = serde_json::json!({
            "session_id": "s", "hook_event_name": "Stop"
        });
        let out = reducer.apply_json(&payload, 1);
        assert!(matches!(
            &out.drafts[0].kind,
            SemanticEventKind::Status { state, .. } if state == "idle"
        ));
    }

    #[test]
    fn unknown_event_produces_nothing() {
        let mut reducer = test_reducer();
        let payload = serde_json::json!({
            "session_id": "s", "hook_event_name": "SomethingNew"
        });
        let out = reducer.apply_json(&payload, 1);
        assert!(out.drafts.is_empty());
        assert!(out.session_binding.is_none());
    }

    #[test]
    fn oversized_tool_input_is_truncated() {
        let mut reducer = test_reducer();
        let big = "x".repeat(100 * 1024);
        let payload = serde_json::json!({
            "session_id": "s", "hook_event_name": "PreToolUse",
            "tool_name": "shell", "tool_input": {"command": big},
            "tool_use_id": "call_big"
        });
        let out = reducer.apply_json(&payload, 1);
        match &out.drafts[0].kind {
            SemanticEventKind::Tool { summary, .. } => {
                assert!(summary.len() <= MAX_CODEX_HOOK_TEXT_BYTES);
                assert!(summary.ends_with("[truncated by DevManager]"));
            }
            other => panic!("expected tool, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn loopback_peer() -> std::net::SocketAddr {
        "127.0.0.1:5555".parse().unwrap()
    }

    fn session_start_body(session_id: &str) -> Vec<u8> {
        serde_json::json!({
            "session_id": session_id,
            "cwd": "C:\\proj",
            "transcript_path": "C:\\sessions\\rollout-a.jsonl",
            "hook_event_name": "SessionStart"
        })
        .to_string()
        .into_bytes()
    }

    fn collecting_handler(
        registry: &CodexHookRegistry,
    ) -> Arc<Mutex<Vec<(CodexHookRegistration, CodexRegistryEvent)>>> {
        let collected = Arc::new(Mutex::new(Vec::new()));
        let sink = collected.clone();
        registry.set_event_handler(Some(Arc::new(move |registration, event| {
            sink.lock().unwrap().push((registration, event));
        })));
        collected
    }

    #[test]
    fn ingest_with_wrong_nonce_is_rejected() {
        let registry = CodexHookRegistry::default();
        let _events = collecting_handler(&registry);
        let _registration = registry.register(StableSessionKey::from_tab("t1")).unwrap();
        assert_eq!(
            registry.ingest(loopback_peer(), "deadbeef", &session_start_body("s"), 1),
            CodexRelayIngestStatus::Rejected
        );
    }

    #[test]
    fn ingest_after_unregister_is_rejected() {
        let registry = CodexHookRegistry::default();
        let _events = collecting_handler(&registry);
        let registration = registry.register(StableSessionKey::from_tab("t1")).unwrap();
        registry.unregister(&registration.nonce);
        assert_eq!(
            registry.ingest(
                loopback_peer(),
                &registration.nonce,
                &session_start_body("s"),
                1
            ),
            CodexRelayIngestStatus::Rejected
        );
    }

    #[test]
    fn stale_generation_cannot_publish() {
        let registry = CodexHookRegistry::default();
        let events = collecting_handler(&registry);
        let first = registry.register(StableSessionKey::from_tab("t1")).unwrap();
        let second = registry.register(StableSessionKey::from_tab("t1")).unwrap();
        assert_eq!(
            registry.ingest(loopback_peer(), &first.nonce, &session_start_body("old"), 1),
            CodexRelayIngestStatus::Rejected
        );
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(
            registry.ingest(loopback_peer(), &second.nonce, &session_start_body("new"), 2),
            CodexRelayIngestStatus::Accepted
        );
        let published = events.lock().unwrap();
        assert!(published
            .iter()
            .all(|(registration, _)| registration.generation == second.generation));
    }

    #[test]
    fn session_start_ingest_emits_binding_event() {
        let registry = CodexHookRegistry::default();
        let events = collecting_handler(&registry);
        let registration = registry.register(StableSessionKey::from_tab("t1")).unwrap();
        assert_eq!(
            registry.ingest(
                loopback_peer(),
                &registration.nonce,
                &session_start_body("s-9"),
                1
            ),
            CodexRelayIngestStatus::Accepted
        );
        let published = events.lock().unwrap();
        assert!(published.iter().any(|(_, event)| matches!(
            event,
            CodexRegistryEvent::SessionStarted(binding)
                if binding.session_id == "s-9"
                    && binding.transcript_path.as_deref()
                        == Some(std::path::Path::new("C:\\sessions\\rollout-a.jsonl"))
        )));
    }

    #[test]
    fn non_loopback_peer_is_rejected() {
        let registry = CodexHookRegistry::default();
        let registration = registry.register(StableSessionKey::from_tab("t1")).unwrap();
        let remote_peer: std::net::SocketAddr = "10.0.0.9:5555".parse().unwrap();
        assert_eq!(
            registry.ingest(remote_peer, &registration.nonce, &session_start_body("s"), 1),
            CodexRelayIngestStatus::Rejected
        );
    }

    #[test]
    fn listener_round_trip_delivers_hook_payload() {
        let registry = Arc::new(CodexHookRegistry::default());
        let events = collecting_handler(&registry);
        let registration = registry.register(StableSessionKey::from_tab("t1")).unwrap();
        let listener = CodexHookRelayListener::start(registry.clone()).unwrap();
        let status = run_codex_hook_relay(
            listener.endpoint(),
            &registration.nonce,
            &session_start_body("live"),
        );
        assert_eq!(status, std::process::ExitCode::SUCCESS);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if events.lock().unwrap().iter().any(|(_, event)| {
                matches!(event, CodexRegistryEvent::SessionStarted(binding) if binding.session_id == "live")
            }) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "hook payload never arrived through the listener"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        drop(listener);
    }
}

#[cfg(test)]
mod launch_builder_tests {
    use super::*;

    #[test]
    fn builds_visible_hook_overrides_only() {
        let command = build_codex_hooks_command(
            "npx -y @openai/codex@latest --yolo",
            "powershell.exe",
            std::path::Path::new(r"C:\Apps\devmanager.exe"),
            "http://127.0.0.1:4321/internal/codex-hook",
            "abc123",
            &[],
        )
        .unwrap();
        assert!(
            command.starts_with("& 'npx' '-y' '@openai/codex@latest' '--yolo'"),
            "user command must lead unchanged: {command}"
        );
        assert!(!command.contains("--remote"));
        assert!(command.contains("--dangerously-bypass-hook-trust"));
        for event in CODEX_HOOK_EVENTS {
            assert!(
                command.contains(&format!("hooks.{event}=")),
                "missing {event} in {command}"
            );
        }
        assert!(command.contains("codex-hook-relay"));
        assert!(command.contains("--nonce abc123"));
    }

    #[test]
    fn toml_basic_string_escapes_backslashes_and_quotes() {
        assert_eq!(toml_basic_string(r#"C:\a "b""#), r#""C:\\a \"b\"""#);
    }

    #[test]
    fn shell_operators_are_rejected() {
        assert!(build_codex_hooks_command(
            "codex --yolo && evil",
            "powershell.exe",
            std::path::Path::new("d.exe"),
            "http://127.0.0.1:1/internal/codex-hook",
            "ff",
            &[],
        )
        .is_err());
    }

    #[test]
    fn non_loopback_endpoint_and_bad_nonce_are_rejected() {
        assert!(build_codex_hooks_command(
            "codex",
            "powershell.exe",
            std::path::Path::new("d.exe"),
            "http://evil.example:1/internal/codex-hook",
            "ff",
            &[],
        )
        .is_err());
        assert!(build_codex_hooks_command(
            "codex",
            "powershell.exe",
            std::path::Path::new("d.exe"),
            "http://127.0.0.1:1/internal/codex-hook",
            "not hex!",
            &[],
        )
        .is_err());
    }

    #[test]
    fn override_value_is_parseable_toml() {
        let command = build_codex_hooks_command(
            "codex --yolo",
            "bash",
            std::path::Path::new(r"C:\Apps\dev manager.exe"),
            "http://127.0.0.1:4321/internal/codex-hook",
            "ff00",
            &[],
        )
        .unwrap();
        // Extract one -c value back out of the bash-quoted command line and
        // confirm the value after `hooks.SessionStart=` parses as TOML.
        let marker = "hooks.SessionStart=";
        let start = command.find(marker).unwrap() + marker.len();
        let rest = &command[start..];
        let end = rest.find("]'").map(|index| index + 1).unwrap();
        let toml_value = &rest[..end];
        let parsed: toml::Value = toml::from_str(&format!("value = {toml_value}")).unwrap();
        let handler = &parsed["value"][0]["hooks"][0];
        assert_eq!(handler["type"].as_str(), Some("command"));
        assert!(handler["command"]
            .as_str()
            .unwrap()
            .contains("codex-hook-relay"));
        assert_eq!(handler["async"].as_bool(), Some(true));
    }
}

#[cfg(test)]
mod relay_cli_tests {
    use super::*;

    #[test]
    fn ignores_other_subcommands() {
        assert!(run_codex_hook_relay_subcommand(&["claude-hook-relay".into()], &b""[..]).is_none());
        assert!(run_codex_hook_relay_subcommand(&[], &b""[..]).is_none());
    }

    #[test]
    fn malformed_args_exit_success_without_posting() {
        let args = vec!["codex-hook-relay".to_string(), "--url".to_string()];
        assert!(run_codex_hook_relay_subcommand(&args, &b"{}"[..]).is_some());
    }

    #[test]
    fn codex_relay_path_is_validated() {
        assert!(is_valid_loopback_relay_url_for(
            "http://127.0.0.1:1234/internal/codex-hook",
            CODEX_HOOK_RELAY_PATH
        ));
        assert!(!is_valid_loopback_relay_url_for(
            "http://127.0.0.1:1234/internal/claude-hook",
            CODEX_HOOK_RELAY_PATH
        ));
    }
}
