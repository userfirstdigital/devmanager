# Codex Hooks Launch Tap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Launch Codex terminals with the user's configured command (plus only visible `-c` hook overrides), restore `codex resume`, and feed the semantic journal from Codex hooks + rollout-file tailing instead of the app-server WebSocket bridge.

**Architecture:** A new `codex-hook-relay` CLI subcommand (mirroring `claude-hook-relay`) receives Codex hook payloads and POSTs them to a loopback endpoint; a registry (nonce + generation fenced, modeled on `ClaudeHookRegistry`) reduces them to `SemanticEventDraft`s. The `SessionStart` hook delivers `transcript_path`; a tailer thread reads that rollout JSONL incrementally and maps records to drafts. The bridge, sidecar, `--remote` injection, version pinning, and npx path resolution are deleted.

**Tech Stack:** Rust; existing crates only (`ureq`, `serde_json`, `getrandom`, std threads). No new dependencies.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-17-codex-hooks-launch-tap-design.md` (read it first).
- A failed tap must never block or alter the user's launch: on any preparation failure, launch the configured command verbatim and mark the adapter Degraded.
- Injected additions to the Codex command are exactly: one `-c hooks.<Event>=[...]` per registered event, plus `--dangerously-bypass-hook-trust` — nothing else. No path resolution, no version pinning, no `--remote`.
- Registered hook events: `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PermissionRequest`, `Stop`.
- Bounded memory discipline: cap per-item text at 64 KiB and total buffered text at 2 MiB (same constants as the current reducer); truncate with the existing `[truncated by DevManager]` convention from `claude_hooks.rs`.
- Verified against Codex 0.144.5: `hooks` feature is stable + default-on; `-c hooks.PreToolUse=[...]` is the accepted key path (value must parse as TOML or Codex treats it as a literal string and errors "expected a sequence"); `--dangerously-bypass-hook-trust` exists on the root and `resume` commands; hook stdin JSON carries `session_id`, `cwd`, `transcript_path`, `hook_event_name`, and for tool events `tool_name`, `tool_input`, `tool_use_id`.
- Rollout record shapes (verified against a real 0.144.5 rollout): JSONL lines `{"timestamp","type","payload"}`; relevant: `event_msg/agent_message {message}`, `event_msg/user_message`, `event_msg/task_started {turn_id}`, `event_msg/task_complete`, `event_msg/turn_aborted`, `event_msg/token_count`, `response_item/reasoning {id, summary:[]}`, `response_item/custom_tool_call {call_id,name,input,status}`, `response_item/custom_tool_call_output {call_id, output:[{type:"input_text",text}]}`, `response_item/function_call {call_id,name,arguments}`, `response_item/function_call_output`, `response_item/message {role, content:[...]}`.
- `cargo test` green and `cargo clippy --all-targets` clean at every commit.

---

### Task 1: `codex-hook-relay` CLI subcommand

**Files:**
- Create: `src/ai/codex_hooks.rs` (new module; add `pub mod codex_hooks;` to `src/ai/mod.rs`)
- Modify: `src/main.rs:6-15`, `src/ai/claude_hooks.rs:2047-2073` (`is_valid_loopback_relay_url`)
- Test: inline `#[cfg(test)]` in `src/ai/codex_hooks.rs`

**Interfaces:**
- Consumes: `claude_hooks::run_hook_relay`-style POST plumbing. Generalize `is_valid_loopback_relay_url(endpoint)` to `is_valid_loopback_relay_url_for(endpoint, expected_path: &str)`; keep the old name as a wrapper passing `"/internal/claude-hook"`.
- Produces: `pub fn run_codex_hook_relay_subcommand<R: Read>(args: &[String], reader: R) -> Option<ExitCode>` recognizing `["codex-hook-relay", "--url", <url>, "--nonce", <nonce>]`, reading stdin (cap: reuse `MAX_CLAUDE_HOOK_BODY_BYTES`, re-export or duplicate as `MAX_CODEX_HOOK_BODY_BYTES` with the same value), POSTing to path `/internal/codex-hook` with header `x-devmanager-codex-nonce`. Constant: `pub const CODEX_HOOK_RELAY_PATH: &str = "/internal/codex-hook";`

- [ ] **Step 1: Write the failing tests**

```rust
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
        assert!(crate::ai::claude_hooks::is_valid_loopback_relay_url_for(
            "http://127.0.0.1:1234/internal/codex-hook",
            CODEX_HOOK_RELAY_PATH
        ));
        assert!(!crate::ai::claude_hooks::is_valid_loopback_relay_url_for(
            "http://127.0.0.1:1234/internal/claude-hook",
            CODEX_HOOK_RELAY_PATH
        ));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test relay_cli_tests` → compile error.

- [ ] **Step 3: Implement.** In `claude_hooks.rs`, rename the body of `is_valid_loopback_relay_url` to `is_valid_loopback_relay_url_for(endpoint, expected_path)` (the final line becomes `path_and_query.path() == expected_path && path_and_query.query().is_none()`), keep the original as a wrapper. In `codex_hooks.rs`, mirror `run_hook_relay_subcommand` (`claude_hooks.rs:2029-2045`) and `run_hook_relay` (`claude_hooks.rs:2011-2027`) exactly, with the codex subcommand name, path constant, and `x-devmanager-codex-nonce` header. In `main.rs`:

```rust
fn main() -> std::process::ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if let Some(exit_code) =
        devmanager::ai::claude_hooks::run_hook_relay_subcommand(&args, std::io::stdin().lock())
    {
        return exit_code;
    }
    if let Some(exit_code) =
        devmanager::ai::codex_hooks::run_codex_hook_relay_subcommand(&args, std::io::stdin().lock())
    {
        return exit_code;
    }
    devmanager::app::run();
    std::process::ExitCode::SUCCESS
}
```

- [ ] **Step 4: Run tests to verify they pass** — `cargo test relay_cli_tests` and full `cargo test`.

- [ ] **Step 5: Commit** — `git commit -m "feat: add codex-hook-relay CLI subcommand"`

---

### Task 2: hook payload reducer

**Files:**
- Modify: `src/ai/codex_hooks.rs`
- Test: inline

**Interfaces:**
- Consumes: `crate::remote::presentation::{SemanticEventDraft, SemanticEventKind, SemanticRetention, SemanticSource, SemanticToolState, StableSessionKey}`.
- Produces:

```rust
pub struct CodexHookReduction {
    pub drafts: Vec<SemanticEventDraft>,
    pub session_binding: Option<CodexSessionBinding>, // Some only for SessionStart
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionBinding {
    pub session_id: String,
    pub transcript_path: Option<std::path::PathBuf>,
    pub cwd: std::path::PathBuf,
}
pub struct CodexHookReducer { /* stable_session_key, seen tool states */ }
impl CodexHookReducer {
    pub fn new(stable_session_key: StableSessionKey) -> Self;
    pub fn apply_json(&mut self, payload: &serde_json::Value, occurred_at_epoch_ms: u64) -> CodexHookReduction;
}
```

- [ ] **Step 1: Write the failing tests** — one per event, driven by literal payloads matching Codex's stdin schema:

```rust
#[test]
fn session_start_produces_binding_and_ready_status() {
    let mut reducer = test_reducer();
    let payload = serde_json::json!({
        "session_id": "019f-abc", "cwd": "C:\\proj",
        "transcript_path": "C:\\Users\\u\\.codex\\sessions\\2026\\07\\17\\rollout-x.jsonl",
        "hook_event_name": "SessionStart", "model": "gpt-5", "permission_mode": "danger-full-access"
    });
    let out = reducer.apply_json(&payload, 1);
    let binding = out.session_binding.unwrap();
    assert_eq!(binding.session_id, "019f-abc");
    assert!(binding.transcript_path.unwrap().ends_with("rollout-x.jsonl"));
    assert!(matches!(out.drafts[0].kind, SemanticEventKind::Status { ref state, .. } if state == "ready"));
}

#[test]
fn permission_request_produces_question() {
    let mut reducer = test_reducer();
    let payload = serde_json::json!({
        "session_id": "019f-abc", "cwd": "C:\\proj", "transcript_path": null,
        "hook_event_name": "PermissionRequest", "model": "gpt-5", "permission_mode": "on-request",
        "tool_name": "shell", "tool_input": {"command": "rm -rf build"}, "tool_use_id": "call_1"
    });
    let out = reducer.apply_json(&payload, 1);
    match &out.drafts[0].kind {
        SemanticEventKind::Question { question_id, prompt, choices } => {
            assert_eq!(question_id, "codex-hook:019f-abc:call_1");
            assert!(prompt.contains("shell"));
            assert_eq!(choices, &vec!["Approve".to_string(), "Decline".to_string()]);
        }
        other => panic!("expected question, got {other:?}"),
    }
}
```

Also: `pre_tool_use_produces_running_tool`, `post_tool_use_completes_tool`, `user_prompt_submit_produces_user_message` (payload field `prompt`), `stop_produces_idle_status`, `unknown_event_produces_nothing`, `oversized_tool_input_is_truncated` (build a 100 KiB `tool_input`, assert the summary is capped at 64 KiB and ends with the truncation marker).

- [ ] **Step 2: Run to verify failure** — compile error.

- [ ] **Step 3: Implement.** Match on `hook_event_name`:
  - `SessionStart` → binding + `Status{state:"ready"}`, dedup key `codex-hook:session:{session_id}`, Canonical.
  - `UserPromptSubmit` → `UserMessage{text}` from `prompt` field, Canonical, dedup `codex-hook:user:{session_id}:{occurred_at_epoch_ms}`.
  - `PreToolUse` → `Tool{tool_id: tool_use_id, name: tool_name, state: Running, summary}` where summary is `tool_input` rendered: if `tool_input.command` is a string use it, else compact JSON; bound to 64 KiB. Dedup `codex-hook:tool:{tool_use_id}`.
  - `PostToolUse` → same tool, `Completed` (`PostToolUseFailure` is not in Codex's event list; failures arrive via rollout outputs).
  - `PermissionRequest` → `Question{question_id: "codex-hook:{session_id}:{tool_use_id}", prompt: "Codex requests permission to run {tool_name}\n\n{summary}", choices: Approve/Decline}`, Canonical, dedup `codex-hook:question:{question_id}`.
  - `Stop` → `Status{state:"idle"}`, dedup `codex-hook:turn-status:{session_id}`.
  - `SemanticSource::Codex` on every draft. Tool-state monotonicity: reuse the `should_advance_tool_state` idea from `claude_hooks.rs:784-792` (copy the small function; do not import claude internals).

- [ ] **Step 4: Run tests** — pass, full suite green.

- [ ] **Step 5: Commit** — `git commit -m "feat: reduce codex hook payloads to semantic events"`

---

### Task 3: rollout line → semantic drafts (pure mapping)

**Files:**
- Create: `src/ai/codex_rollout.rs` (add `pub mod codex_rollout;` to `src/ai/mod.rs`)
- Test: inline, fixture strings copied from the Global Constraints record shapes

**Interfaces:**
- Produces:

```rust
pub struct CodexRolloutReducer { /* stable_session_key, bounded text budget like CodexSemanticReducer */ }
impl CodexRolloutReducer {
    pub fn new(stable_session_key: StableSessionKey) -> Self;
    /// One JSONL line (no trailing newline). Malformed/unknown lines yield no drafts.
    pub fn observe_line(&mut self, line: &str, observed_at_epoch_ms: u64) -> Vec<SemanticEventDraft>;
}
```

- [ ] **Step 1: Write the failing tests** using literal lines, e.g.:

```rust
#[test]
fn agent_message_maps_to_assistant() {
    let mut reducer = test_reducer();
    let line = r#"{"timestamp":"2026-07-17T17:37:37.799Z","type":"event_msg","payload":{"type":"agent_message","message":"Working on it.","phase":"commentary"}}"#;
    let drafts = reducer.observe_line(line, 5);
    assert!(matches!(&drafts[0].kind, SemanticEventKind::AssistantMessage { text, streaming, .. } if text == "Working on it." && !streaming));
}

#[test]
fn custom_tool_call_maps_to_command() {
    let mut reducer = test_reducer();
    let line = r#"{"timestamp":"t","type":"response_item","payload":{"type":"custom_tool_call","id":"ctc_1","status":"completed","call_id":"call_9","name":"exec","input":"echo hi"}}"#;
    let drafts = reducer.observe_line(line, 5);
    assert!(matches!(&drafts[0].kind, SemanticEventKind::Command { command_id, text, .. } if command_id == "call_9" && text == "echo hi"));
}
```

Also: `reasoning_with_summary_maps_to_reasoning` (summary array joined; empty summary → no draft), `task_started_maps_to_working_status`, `task_complete_maps_to_idle_status`, `turn_aborted_maps_to_interrupted_status`, `token_count_maps_to_verbose_usage_status`, `tool_call_output_maps_to_output` (join `output[].text`, Verbose retention), `function_call_maps_to_tool`, `user_and_developer_response_messages_are_skipped` (UserPromptSubmit hook already covers user text; developer messages are internal), `malformed_line_yields_nothing`, `unknown_types_yield_nothing`, `oversized_message_is_truncated` (64 KiB cap, marker suffix).

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement.** Parse `{type, payload}`; dedup keys `codex-rollout:{kind}:{id-or-call_id-or-turn_id}`. Assistant messages: prefer `event_msg/agent_message`; skip `response_item/message` with role `assistant` **when an agent_message with identical text was just emitted** — simplest correct rule: emit `response_item/message role=assistant` with dedup key `codex-rollout:assistant:{payload.id}` and let the journal's dedup handle overlap; never concatenate. Copy `truncate_utf8` and the byte-budget pattern from `codex_bridge.rs` (`append_item`/`enforce_total_limit`, `codex_bridge.rs:757-837`) — these move here in Task 8; duplicating now is fine, Task 8 deletes the originals.

- [ ] **Step 4: Run tests** — pass.

- [ ] **Step 5: Commit** — `git commit -m "feat: map codex rollout records to semantic events"`

---

### Task 4: rollout tailer

**Files:**
- Modify: `src/ai/codex_rollout.rs`
- Test: inline (temp files)

**Interfaces:**
- Consumes: `CodexRolloutReducer` (Task 3).
- Produces:

```rust
pub struct CodexRolloutTailer { /* shutdown flag + join handle */ }
impl CodexRolloutTailer {
    /// Spawns a thread that polls `path` every 250ms, reads complete new lines
    /// (buffering partial trailing lines), runs them through the reducer, and
    /// calls `on_event` per draft. Starts from the file's beginning.
    pub fn start<F>(path: PathBuf, stable_session_key: StableSessionKey, on_event: F) -> Self
    where F: Fn(SemanticEventDraft) + Send + Sync + 'static;
    pub fn stop(self); // signals shutdown, joins the thread
}
```

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn tailer_emits_events_for_appended_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    std::fs::write(&path, "").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let tailer = CodexRolloutTailer::start(path.clone(), test_key(), move |draft| { let _ = tx.send(draft); });
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    use std::io::Write as _;
    writeln!(file, r#"{{"timestamp":"t","type":"event_msg","payload":{{"type":"agent_message","message":"hello"}}}}"#).unwrap();
    let draft = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(matches!(draft.kind, SemanticEventKind::AssistantMessage { .. }));
    tailer.stop();
}
```

Also: `partial_line_is_not_emitted_until_newline` (write half a line, assert no event within 600ms, complete it, assert event), `missing_file_retries_without_panicking` (start on a nonexistent path, create the file after 300ms, append, assert event), `stop_joins_cleanly`.

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement.** Thread loop: open (retry while `NotFound` and not shut down), track read offset, `metadata().len()` shrink ⇒ file was rotated: reopen from 0 with a fresh reducer. Read with a `BufReader` from the saved offset; only process buffered data up to the last `\n`. Cap a single line at 16 MiB (skip oversized lines and resync at the next newline). Use `epoch_millis` (copy the 6-line helper from `codex_bridge.rs:1328-1335`).

- [ ] **Step 4: Run tests** — pass (verify no test takes > 10s).

- [ ] **Step 5: Commit** — `git commit -m "feat: tail codex rollout files into semantic events"`

---

### Task 5: launch command builder

**Files:**
- Modify: `src/ai/codex_hooks.rs`
- Test: inline

**Interfaces:**
- Consumes: `quote_command_for_shell(tokens, shell_program)` and `help_advertises_flag(help, flag)` — copy both from `codex_bridge.rs:2139` / `:1992-2003` into `codex_hooks.rs` (Task 8 deletes the originals). Also `split_command_line` from `codex_bridge.rs:2077` for validation.
- Produces:

```rust
pub const CODEX_HOOK_EVENTS: &[&str] = &[
    "SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PermissionRequest", "Stop",
];
/// Capability probe: resolves the command's executable (move `resolve_executable`,
/// `executable_candidate_names`, and `run_probe` here from codex_bridge.rs), runs
/// `<exe> [prefix args] --help`, and requires `help_advertises_flag(help,
/// "--dangerously-bypass-hook-trust")`. Err = launch verbatim + Degraded (old
/// Codex would hard-fail on the unknown flag, so never inject without this).
pub fn codex_supports_hooks(startup_command: &str) -> Result<(), String>;
/// Renders a TOML basic string: wraps in double quotes, escapes \ and ".
pub fn toml_basic_string(value: &str) -> String;
/// The full command line for the PTY: user command + hook -c overrides
/// + --dangerously-bypass-hook-trust. Errors reject unsafe commands
/// (shell operators / unterminated quotes) — caller then launches verbatim.
pub fn build_codex_hooks_command(
    startup_command: &str,
    shell_program: &str,
    devmanager_executable: &std::path::Path,
    endpoint: &str,   // http://127.0.0.1:{port}/internal/codex-hook
    nonce: &str,
) -> Result<String, String>;
```

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn builds_visible_hook_overrides_only() {
    let command = build_codex_hooks_command(
        "npx -y @openai/codex@latest --yolo",
        "powershell.exe",
        std::path::Path::new(r"C:\Apps\devmanager.exe"),
        "http://127.0.0.1:4321/internal/codex-hook",
        "abc123",
    ).unwrap();
    assert!(command.starts_with("npx -y @openai/codex@latest --yolo"));
    assert!(!command.contains("--remote"));
    assert!(command.contains("--dangerously-bypass-hook-trust"));
    for event in CODEX_HOOK_EVENTS {
        assert!(command.contains(&format!("hooks.{event}=")), "missing {event}");
    }
    assert!(command.contains("codex-hook-relay"));
    assert!(command.contains("--nonce abc123") || command.contains("--nonce 'abc123'") || command.contains("--nonce \\\"abc123\\\""));
}

#[test]
fn toml_basic_string_escapes_backslashes_and_quotes() {
    assert_eq!(toml_basic_string(r#"C:\a "b""#), r#""C:\\a \"b\"""#);
}

#[test]
fn shell_operators_are_rejected() {
    assert!(build_codex_hooks_command(
        "codex --yolo && evil", "powershell.exe",
        std::path::Path::new("d.exe"), "http://127.0.0.1:1/internal/codex-hook", "n",
    ).is_err());
}
```

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement.** Per event, the TOML override value is:

```text
hooks.<Event>=[{hooks=[{type="command",command=<toml_basic_string(relay_command)>,async=true}]}]
```

where `relay_command` is `"{exe}" codex-hook-relay --url {endpoint} --nonce {nonce}` with the exe path double-quoted (Codex runs `command` through a shell; double quotes are safe on cmd/PowerShell/sh). Assemble final tokens = `split_command_line(startup_command)?` + for each event `["-c", override]` + `["--dangerously-bypass-hook-trust"]`, then `quote_command_for_shell(&tokens, shell_program)`. Validation: reuse `split_command_line`'s rejection of `| ; & < > \r \n` and unterminated quotes. Do NOT resolve the executable and do NOT touch the package version token.

- [ ] **Step 4: Run tests** — pass.

- [ ] **Step 5: Commit** — `git commit -m "feat: build codex launch command with hook overrides"`

---

### Task 6: codex hook registry + relay listener route

**Files:**
- Modify: `src/ai/codex_hooks.rs`, `src/ai/claude_hooks.rs` (listener routing only)
- Test: inline in `codex_hooks.rs`

**Interfaces:**
- Consumes: `ClaudeHookRelayListener` internals (`claude_hooks.rs`, search `ClaudeHookRelayListener::start` and its request loop) — it currently accepts POSTs to `/internal/claude-hook` only.
- Produces:
  - `pub struct CodexHookRegistry` mirroring `ClaudeHookRegistry`'s shape (`claude_hooks.rs:180` region): `register_at(stable_session_key, now) -> Result<CodexHookRegistration, String>` (nonce + generation), `unregister(&nonce)`, `ingest_at(nonce, body, now)` which validates nonce/generation, parses JSON, runs `CodexHookReducer`, and invokes the registered event handler with `(registration, CodexRegistryEvent)`.
  - `pub enum CodexRegistryEvent { Semantic(SemanticEventDraft), SessionStarted(CodexSessionBinding), RegistrationDropped { reason: String } }`
  - Listener: extend the relay HTTP listener to route by path — `/internal/claude-hook` → claude registry ingest (unchanged), `/internal/codex-hook` + `x-devmanager-codex-nonce` header → codex registry ingest. The listener constructor gains an `Option<Arc<CodexHookRegistry>>` parameter; existing call sites pass the new registry.

- [ ] **Step 1: Write the failing tests** — model directly on the existing claude registry tests in `claude_hooks.rs` (find them by searching `register_at` in the test module): `ingest_with_wrong_nonce_is_rejected`, `ingest_after_unregister_is_rejected`, `stale_generation_cannot_publish` (register twice for the same `StableSessionKey`, assert the first nonce's ingest is dropped), `session_start_ingest_emits_binding_event`.

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement** following the claude registry as the reference implementation — same locking, same generation counter approach, same drop-cleanup behavior. Route by exact path match in the listener request handler.

- [ ] **Step 4: Run tests** — pass, plus the existing claude relay tests still green.

- [ ] **Step 5: Commit** — `git commit -m "feat: codex hook registry with relay listener route"`

---

### Task 7: process manager integration

**Files:**
- Modify: `src/services/process_manager.rs` — replace the bridge flow inside `prepare_codex_launch_for_session` (`:1485+`) and its launch call sites (`:1642`, `:1702`), reusing the session bookkeeping that exists for claude (`ClaudeHookSession` pattern at `:177-186`, `:1360-1433`).
- Test: existing `process_manager` test module, following the claude-session tests there.

**Interfaces:**
- Consumes: Task 5 `build_codex_hooks_command`, Task 6 `CodexHookRegistry`/`CodexRegistryEvent`, Task 4 `CodexRolloutTailer`, existing `claude_hook_endpoint()`-style listener startup (`:1343-1358`), `emit_remote_session_event`, `RemoteSessionEvent::{CodexSemantic, CodexAdapterRegistered, CodexAdapterRemoved, AdapterHealth}`.
- Produces: a codex launch path that:
  0. Calls `codex_supports_hooks(startup_command)` (Task 5); on `Err`, launches the configured command verbatim and emits `AdapterHealth` Degraded — steps 1-5 are skipped.
  1. Starts/gets the relay listener endpoint (codex path variant of `claude_hook_endpoint`).
  2. Registers with `CodexHookRegistry` for the tab's `StableSessionKey`.
  3. Builds the command via `build_codex_hooks_command`; on `Err`, launches `startup_command` verbatim and emits `AdapterHealth` Degraded with the error string.
  4. Stores a `CodexHookSession { registration, tailer: Option<CodexRolloutTailer> }` keyed by session id (mirror `claude_hook_sessions`).
  5. On `CodexRegistryEvent::SessionStarted(binding)`: if `binding.transcript_path` is `Some`, start a `CodexRolloutTailer` publishing drafts through the same `RemoteSessionEvent::CodexSemantic` channel; emit `CodexAdapterRegistered` and `AdapterHealth` Healthy. A second `SessionStarted` for the same session (resume) stops the old tailer and starts one on the new path.
  6. On session stop/close/restart: `fence_and_remove` mirror of the claude version (`:363-405`) — unregister nonce, stop tailer, emit `CodexAdapterRemoved`.

- [ ] **Step 1: Write the failing tests** — using the module's existing test seams (`set_codex_adapter_preparer_for_test` and friends get replaced; add equivalents): `codex_launch_command_contains_hook_overrides_and_no_remote`, `codex_launch_falls_back_verbatim_on_builder_error`, `session_start_event_starts_tailer_and_reports_healthy`, `closing_session_unregisters_and_stops_tailer`, `superseded_relaunch_cannot_publish` (reuse the claude fencing test shape).

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement.** Delete the bridge-specific pieces this flow replaces as you go **only where they block compilation**; the bulk removal is Task 8. Remove: `CodexAdapterRegistry` consultation in the launch path, activation-timeout plumbing, `CodexFallbackTerminalOps` remote-command injection (`AI_COMMAND_INJECTION_DELAY_MS` handling for codex), and the `prepare_codex_adapter` preparer indirection.

- [ ] **Step 4: Run tests** — new tests pass; run the full `cargo test` and fix fallout in remote/web tests that asserted bridge behavior (update them to the hooks flow, keeping their intent: health transitions, fencing, semantic publication).

- [ ] **Step 5: Commit** — `git commit -m "feat: launch codex via hooks tap instead of websocket bridge"`

---

### Task 8: delete the bridge

**Files:**
- Modify: `src/ai/codex_bridge.rs` (mostly deleted), `src/ai/mod.rs`, `src/services/process_manager.rs`, any `remote` module references
- Test: full suite

**Interfaces:**
- Consumes: nothing new. Everything still referenced from `codex_bridge.rs` after Task 7 must move: `CodexSemanticReducer` is superseded by `CodexRolloutReducer` — delete it and its tests; keep and relocate only helpers now owned by `codex_hooks.rs`/`codex_rollout.rs` (`quote_command_for_shell`, `help_advertises_flag`, `strip_ansi_csi`, `split_command_line`, `truncate_utf8`, `epoch_millis` — delete the `codex_bridge` originals so each exists exactly once, in its new home).

- [ ] **Step 1: Delete** `serve_one_loopback_client*`, `CodexBridgeHandle`, `StartedCodexBridge`, `prepare_codex_adapter*`, `PreparedCodexAdapter`, `parse_codex_command`, version pinning machinery (`parse_codex_version`, `validate_version_token`; NOTE `run_probe`, `resolve_executable`, and `executable_candidate_names` are NOT deleted — they moved to `codex_hooks.rs` in Task 5 for `codex_supports_hooks`), `SemanticObserver*` channel types, `forward_server_frame`, `peer_is_allowed`, `random_bridge_token`, `constant_time_eq`, `authorize_bridge_handshake`, `read_jsonl_frame`, `trace_codex_bridge_frame`, `initialize_request_id/response`, `CODEX_BRIDGE_AUTH_TOKEN_ENV`, and all their tests. If the file empties, delete it and remove the module declaration; `CodexConfigOverride` moves to `codex_hooks.rs` **only if** the browser-overlay call site (`codex_browser_config_overrides`, imported at `process_manager.rs:7`) still needs it — check first; if it does, `build_codex_hooks_command` gains a `config: &[CodexConfigOverride]` parameter appended as `--config key=value` pairs exactly like `tui_command_with_config` did (`codex_bridge.rs:1845-1863`), and Task 7's call site passes the browser overrides through.

- [ ] **Step 2: Build and fix** — `cargo build` then `cargo test`; every remaining reference is either moved or was dead.

- [ ] **Step 3: Verify no bridge remnants** — `rg -n "app-server|--remote|BRIDGE|sidecar" src/` returns only comments/docs that describe history (update or delete them) and the unrelated `remote` module namespace.

- [ ] **Step 4: Commit** — `git commit -m "refactor: remove codex app-server websocket bridge"`

---

### Task 9: Claude audit + settings help text

**Files:**
- Modify: `src/ai/claude_hooks.rs` (tests only unless a bug is found), `src/workspace/mod.rs:1926-1940` (command help text)
- Test: inline in `claude_hooks.rs`

- [ ] **Step 1: Write passthrough tests** for `prepare_claude_launch_overlay` (see its signature at `claude_hooks.rs:2147`): `resume_flag_passes_through` (`npx -y @anthropic-ai/claude-code@latest --resume --dangerously-skip-permissions` → overlay command still contains `--resume` unchanged, plus exactly one `--settings`), `continue_flag_passes_through`, `user_settings_argument_is_merged_not_duplicated` (already covered? search existing tests for `find_settings_argument`; add only what's missing).

- [ ] **Step 2: Run** — expected to pass immediately (this is an audit); if any fails, that's a real bug — fix minimally and note it in the commit message.

- [ ] **Step 3: Update settings help text** in `src/workspace/mod.rs` for both command fields:
  - Claude field description: `Command used for Claude terminals. DevManager appends --settings <temp file> to relay conversation events.`
  - Codex field description: `Command used for Codex terminals. DevManager appends -c hook overrides and --dangerously-bypass-hook-trust to relay conversation events.`

- [ ] **Step 4: Run full suite + clippy** — green.

- [ ] **Step 5: Commit** — `git commit -m "test: audit claude launch passthrough; document injected args"`

---

### Task 10: end-to-end manual QA

- [ ] **Step 1:** `cargo run`; launch a Codex terminal from the button. Verify the visible command is the configured command + `-c hooks....` overrides + `--dangerously-bypass-hook-trust` and nothing else.
- [ ] **Step 2:** In the session, ask Codex to run a command; verify the mobile remote view shows the assistant text, a tool/command card, and (with approval policy `on-request`) a live approval question the moment it's asked; answer it from the phone view.
- [ ] **Step 3:** Exit the TUI. Run `codex resume` manually in a plain terminal in the same cwd — the DevManager-launched session must appear in the picker and resume.
- [ ] **Step 4:** Relaunch the Codex tab (restart) and confirm the old session's events don't republish into the new session (generation fencing) and the tailer follows the new rollout file.
- [ ] **Step 5:** Point the Codex command at an old version without hooks support (e.g. `npx -y @openai/codex@0.90.0 --yolo` temporarily) and verify the terminal still launches verbatim with the adapter reported Degraded.
