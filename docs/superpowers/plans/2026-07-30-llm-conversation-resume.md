# Exact LLM Conversation Resume Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist Claude/Codex provider conversation IDs on open tabs and resume
the exact saved conversation automatically after DevManager restarts.

**Architecture:** Provider `SessionStart` hooks bind an authoritative provider
ID to the matching PTY runtime. The existing background maintenance pass
projects that runtime fact into `SessionTab.provider_session_id` and persists
`session.json`; launch construction then chooses fresh, exact-resume, or legacy
resume-picker behavior from the tab's provider and PTY identities.

**Tech Stack:** Rust, Serde JSON, GPUI application state, existing Claude/Codex
hook relays, existing process-manager/runtime revision system.

## Global Constraints

- Never infer a provider conversation from cwd, timestamps, transcript
  directories, or "latest" state.
- Never silently fall back from an exact-resume failure to a fresh
  conversation.
- New tabs start fresh; saved tabs with an ID resume exactly; legacy saved tabs
  without an ID open the provider picker once.
- Hook callbacks remain independent from `AppState`, disk I/O, and rendering.
- Preserve configured Claude/Codex wrapper commands and existing hook/settings
  injection.
- Tests and development binaries must not touch the installed DevManager
  process or production config.
- Run the complete library suite only as
  `cargo test --lib -- --test-threads=1`.

---

### Task 1: Durable tab identity

**Files:**
- Modify: `src/models/config.rs`
- Modify: `src/state/app_state.rs`
- Modify: all existing `SessionTab` struct literals reported by
  `rg -n "SessionTab \\{" src`
- Test: `src/models/config.rs`
- Test: `src/state/app_state.rs`

**Interfaces:**
- Produces: `SessionTab.provider_session_id: Option<String>`
- Produces:
  `AppState::update_ai_tab_provider_session(&mut self, tab_id: &str, provider_session_id: String) -> bool`
- Consumes: existing Serde `camelCase` and `#[serde(default)]` behavior

- [ ] **Step 1: Write failing compatibility and state-mutation tests**

Add a Serde test proving an old tab without `providerSessionId` deserializes to
`None`, then set the field and assert the JSON contains the literal
`"providerSessionId":"provider-123"`. Add `AppState` tests proving an LLM tab is
updated, a server tab is rejected, and assigning the same value twice does not
bump `AppState::revision()`.

```rust
#[test]
fn session_tab_provider_identity_is_backward_compatible() {
    let mut tab: SessionTab = serde_json::from_str(
        r#"{"id":"ai","type":"claude","projectId":"p"}"#,
    )
    .unwrap();
    assert_eq!(tab.provider_session_id, None);

    tab.provider_session_id = Some("provider-123".to_string());
    let encoded = serde_json::to_string(&tab).unwrap();
    assert!(encoded.contains(r#""providerSessionId":"provider-123""#));
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```powershell
cargo test --lib session_tab_provider_identity_is_backward_compatible -- --exact
cargo test --lib update_ai_tab_provider_session -- --nocapture
```

Expected: compile failure because the field and method do not exist.

- [ ] **Step 3: Add the durable field and idempotent update method**

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub provider_session_id: Option<String>,
```

The update method must return `false` for non-LLM/missing tabs and unchanged
values. It sets the field, calls `mark_dirty()`, and returns `true` only on an
actual change. Add `provider_session_id: None` to all pre-existing struct
literals; LLM fixtures that specifically exercise resume may override it.

- [ ] **Step 4: Run focused model/state tests and verify GREEN**

Run:

```powershell
cargo test --lib session_tab_provider_identity_is_backward_compatible -- --exact
cargo test --lib update_ai_tab_provider_session -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add src/models/config.rs src/state/app_state.rs src
git commit -m "feat: persist llm provider session identity"
```

### Task 2: Provider-aware launch modes

**Files:**
- Modify: `src/services/process_manager.rs`
- Test: `src/services/process_manager.rs`

**Interfaces:**
- Consumes: `SessionTab.provider_session_id`
- Produces:
  `build_ai_launch_spec(settings: &Settings, project: &Project, tab: &SessionTab, session_id: &str) -> Result<AiLaunchSpec, String>`
- Produces private helpers that validate a provider ID and safely append provider
  resume arguments using the selected interactive shell.

- [ ] **Step 1: Write failing launch-command tests**

Build literal Claude and Codex tabs against fixed settings/project fixtures and
assert:

```rust
// Fresh tab: created with a PTY ID, no provider ID.
assert_eq!(fresh.startup_command, configured);

// Exact saved tab: restored PTY ID is None, provider ID is present.
assert!(claude_exact.startup_command.contains("--resume"));
assert!(claude_exact.startup_command.contains("provider-123"));
assert!(codex_exact.startup_command.contains("resume"));
assert!(codex_exact.startup_command.contains("provider-123"));

// Legacy saved tab: both IDs are absent.
assert!(claude_legacy.startup_command.ends_with("--resume"));
assert!(codex_legacy.startup_command.ends_with("resume"));
```

Also assert a provider ID containing `\r`, `\n`, a shell separator, or more than
256 characters returns an error containing `provider session id`.

- [ ] **Step 2: Run launch tests and verify RED**

Run:

```powershell
cargo test --lib ai_launch_ -- --nocapture
```

Expected: assertions fail because all modes currently use the configured
command unchanged.

- [ ] **Step 3: Implement the three launch modes**

Resolve the interactive shell before adapting the startup command. Use this
decision exactly:

```rust
let resume = match (
    tab.provider_session_id.as_deref(),
    tab.pty_session_id.as_deref(),
) {
    (Some(id), _) => AiResume::Exact(validate_provider_session_id(id)?),
    (None, None) => AiResume::Picker,
    (None, Some(_)) => AiResume::Fresh,
};
```

For Claude append `--resume` plus the quoted exact ID when present. For Codex
append `resume` plus the quoted exact ID when present. Fresh mode returns the
configured command byte-for-byte. Use the existing shell-kind/quoting helpers;
do not concatenate an unvalidated persisted ID.

- [ ] **Step 4: Run launch tests and verify GREEN**

Run:

```powershell
cargo test --lib ai_launch_ -- --nocapture
```

Expected: all fresh/exact/picker/invalid tests PASS.

- [ ] **Step 5: Commit**

```powershell
git add src/services/process_manager.rs
git commit -m "feat: resume saved claude and codex conversations"
```

### Task 3: Capture authoritative IDs in runtime state

**Files:**
- Modify: `src/state/runtime_state.rs`
- Modify: `src/ai/claude_hooks.rs`
- Modify: `src/services/process_manager.rs`
- Test: `src/state/runtime_state.rs`
- Test: `src/ai/claude_hooks.rs`
- Test: `src/services/process_manager.rs`

**Interfaces:**
- Produces: `SessionRuntimeState.provider_session_id: Option<String>`
- Produces:
  `ClaudeRegistryEvent::SessionStarted { provider_session_id: String }`
- Consumes: existing `CodexRegistryEvent::SessionStarted(CodexSessionBinding)`

- [ ] **Step 1: Write failing hook/runtime tests**

Add tests proving:

- `SessionRuntimeState::new` starts with no provider ID and `configure_ai`
  clears stale identity.
- An accepted current-generation Claude `SessionStart` publishes the bounded
  official ID once, while a stale registration cannot publish it.
- Claude and Codex session-start handlers write the provider ID onto the
  correlated internal runtime.
- Re-delivering the same provider ID does not increase `runtime_revision()`,
  while changing it does.

The hook tests must invoke the real registry reduction/event flow rather than
asserting on a mock handler's existence.

- [ ] **Step 2: Run focused hook/runtime tests and verify RED**

Run:

```powershell
cargo test --lib provider_session_id -- --nocapture
cargo test --lib session_start_publishes -- --nocapture
```

Expected: compile failures because runtime/provider events do not exist.

- [ ] **Step 3: Publish Claude session identity**

Parse `SessionStart` once in `ClaudeHookRegistry::reduce_admitted`, retain the
official bounded `session_id` in `CapturedClaudeIngest`, and publish
`ClaudeRegistryEvent::SessionStarted` only after the existing
current-registration validation. Do not use the reducer fallback identifier as
a durable provider ID.

```rust
SessionStarted {
    provider_session_id: String,
}
```

- [ ] **Step 4: Bind Claude and Codex events into runtime**

Add one process-manager helper that updates the matching
`SessionRuntimeState.provider_session_id`, marks the runtime dirty, bumps the
runtime revision, and emits the existing remote runtime snapshot only when the
value changes. Call it from:

- the correlated Claude registry handler using
  `ClaudeSemanticIdentity.pty_session_id`;
- `handle_codex_session_started` using `binding.session_id` before moving the
  transcript path into the tailer.

- [ ] **Step 5: Run focused hook/runtime tests and verify GREEN**

Run:

```powershell
cargo test --lib provider_session_id -- --nocapture
cargo test --lib session_start_publishes -- --nocapture
```

Expected: PASS.

- [ ] **Step 6: Commit**

```powershell
git add src/state/runtime_state.rs src/ai/claude_hooks.rs src/services/process_manager.rs
git commit -m "feat: capture llm provider sessions from hooks"
```

### Task 4: Project runtime identity into saved tabs

**Files:**
- Modify: `src/app/mod.rs`
- Test: `src/app/mod.rs`

**Interfaces:**
- Consumes: `RuntimeState.sessions[*].tab_id`
- Consumes: `RuntimeState.sessions[*].provider_session_id`
- Produces:
  `sync_ai_provider_sessions(state: &mut AppState, runtime: &RuntimeState) -> bool`

- [ ] **Step 1: Write failing projection and persistence tests**

Create a pure helper test with two open LLM tabs and runtime entries for only
one tab. Assert only the matching tab receives the provider ID and the helper
returns `true`; the second identical call returns `false`. Extend
`persisted_session_state_keeps_ai_workspace_but_strips_runtime_identity` to
assert `pty_session_id == None` while `provider_session_id` is preserved.

- [ ] **Step 2: Run app tests and verify RED**

Run:

```powershell
cargo test --lib sync_ai_provider_sessions -- --nocapture
cargo test --lib persisted_session_state_keeps_ai_workspace_but_strips_runtime_identity -- --exact
```

Expected: compile/assertion failure because projection/preservation is absent.

- [ ] **Step 3: Implement projection outside the hot path**

The pure helper collects `(tab_id, provider_session_id)` pairs from AI runtime
entries and calls `AppState::update_ai_tab_provider_session`. Invoke it from
`refresh_remote_host_maintenance` immediately after taking the single runtime
snapshot:

```rust
let runtime_state = self.process_manager.runtime_state();
if sync_ai_provider_sessions(&mut self.state, &runtime_state) {
    self.save_session_state();
}
```

Keep `retain_startup_restorable_tabs` clearing only `command_id` and
`pty_session_id`; it must not clear `provider_session_id`.

- [ ] **Step 4: Run focused app tests and verify GREEN**

Run:

```powershell
cargo test --lib sync_ai_provider_sessions -- --nocapture
cargo test --lib persisted_session_state_keeps_ai_workspace_but_strips_runtime_identity -- --exact
```

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add src/app/mod.rs
git commit -m "feat: save captured llm conversation identity"
```

### Task 5: Integrated verification and durable invariant

**Files:**
- Modify if warranted after overlap review: `AGENTS.md`
- Review: all branch changes since `0578ee0`

**Interfaces:**
- Consumes: all prior tasks
- Produces: verified isolated branch with installed-app safety evidence

- [ ] **Step 1: Run formatting only on edited Rust files**

Run:

```powershell
rustfmt --edition 2021 src/models/config.rs src/state/app_state.rs src/state/runtime_state.rs src/ai/claude_hooks.rs src/services/process_manager.rs src/app/mod.rs
```

Revert any formatter-only edits outside those paths.

- [ ] **Step 2: Run focused provider-resume tests together**

Run:

```powershell
cargo test --lib provider_session -- --nocapture
cargo test --lib ai_launch_ -- --nocapture
cargo test --lib sync_ai_provider_sessions -- --nocapture
```

Expected: PASS with no warnings introduced by this feature.

- [ ] **Step 3: Review the complete diff**

Run:

```powershell
git diff --check 0578ee0...HEAD
git diff --stat 0578ee0...HEAD
git diff 0578ee0...HEAD
```

Confirm every `SessionTab` literal has an intentional provider identity,
fresh-launch commands remain unchanged, exact resume cannot fall back, hook
correlation is generation-safe, and persistence does not run on hook/render
threads.

- [ ] **Step 4: Run the full serial library suite**

Tell the user that Rust test harness executables will appear temporarily, then
run:

```powershell
cargo test --lib -- --test-threads=1
```

Expected: all library tests PASS.

- [ ] **Step 5: Verify installed-app isolation**

Confirm:

- installed `devmanager.exe` is still PID `44880` with start time
  `2026-07-23 14:20:16` local;
- production `config.json` SHA-256 is
  `7BB8C5A5344443DD1D0CAC6068E332EDC5A35DDB9AF9EEE0EFC77F45D0A1BDC5`;
- production `remote.json` SHA-256 is
  `3F3756F8F298463A2938DC7D85D3DF9F3011B0D8BA208AA81E27C11A0DD1D8A9`;
- no Cargo, `rustc`, or `target\debug\deps` test harness remains.

- [ ] **Step 6: Apply Sharpening the Axe**

Search `AGENTS.md` and project guidance for an overlapping invariant. If no
equivalent exists and implementation evidence confirms it, add one concise
project rule: provider conversation identity is distinct from PTY identity;
capture it only from correlated provider hooks, preserve it for open tabs, and
never infer it from cwd/latest transcripts. Do not add a duplicate rule.

- [ ] **Step 7: Commit final corrections or guidance**

```powershell
git add <only-reviewed-files>
git commit -m "docs: preserve llm conversation identity invariant"
```

Skip this commit when no guidance change or correction is needed.
