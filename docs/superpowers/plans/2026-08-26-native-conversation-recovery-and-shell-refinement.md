# Native Conversation Recovery and Shell Refinement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make native conversations acknowledge input promptly, survive restart with their exact provider identity and local history, expose accurate terminal/activity/sound state, and correct the compact composer and task-rail interactions.

**Architecture:** Keep the durable semantic journal and exact provider-session binding as authorities, while separating runtime attachment health from task lifecycle and cached transcript state. Admit only monotonic conversation deltas into per-task surfaces, preserve terminal snapshots across reconnects, and reuse existing v0.4.1 status/sound behavior. Native GPUI controls become explicit compact popovers and the rail receives focused layout/navigation corrections.

**Tech Stack:** Rust 2021, GPUI 0.2.2, SQLite-backed command/event projections, existing DevManager host protocol and semantic journal, Windows native acceptance harnesses.

**Spec:** `docs/superpowers/specs/2026-08-26-native-conversation-recovery-and-shell-refinement-design.md`

## Global Constraints

- `ProviderSessionId` is captured only from correlated current-generation hooks, persisted exactly, and never synthesized or inferred.
- A stored provider session ID never silently falls back to a new conversation.
- Conversation history remains profile-local semantic-journal truth; PTY bytes never become semantic transcript truth.
- Restore/attachment health is distinct from durable task failure and from the Settled/Done lifecycle.
- Preserve multi-folder projects, recursive multi-task panes, focus/action/runtime fences, and installed-app isolation.
- Preserve the user's uncommitted `AGENTS.md`; stage only files owned by this work.
- Use the current checkout for hot reload. Every Cargo command uses `CARGO_TARGET_DIR=C:\Temp\devmanager-native-multi-task-VisualDevManager`.
- Run focused RED/GREEN checks during implementation and the broad Rust train once after edits freeze.

---

## File structure

- Modify `src/host/connection.rs`: exact restore classification, runtime attachment outcome, native sound configuration, and host tests.
- Modify `src/kernel/command_bus.rs`, `src/kernel/projector.rs`, and/or their existing tests only if the end-to-end provider-session round trip has an actual gap.
- Modify `src/domain/cockpit.rs` and `src/domain/snapshot.rs` only for the smallest backward-compatible attachment/change state needed at the cockpit boundary.
- Modify `src/ui/task_workspace/surfaces.rs`: monotonic delta cache, change reporting, terminal snapshot/attachment state.
- Modify `src/ui/task_cockpit/shell.rs` and `src/ui/task_cockpit/timeline.rs`: keep cached history visible under attachment failure and avoid unchanged full reprojection.
- Modify `src/ui/native_shell.rs`: optimistic admission, bounded delta scheduling, status/terminal projection, compact popovers, rail controls, scroll geometry, and archived selection.
- Modify `src/services/process_manager.rs` only if existing status/sound transition behavior needs a focused correctness repair.
- Modify existing tests beside each production surface; do not create source-grep tests or mock-only assertions.

---

### Task 1: Exact conversation identity and restart attachment state

**Files:**
- Modify: `src/host/connection.rs`
- Test/modify if needed: `src/kernel/command_bus.rs`, `src/kernel/projector.rs`, `src/kernel/store.rs`

**Interfaces:**
- Consumes: durable agent `provider_session_id`, current provider capability/launch identity, exact resource/runtime fences.
- Produces: a restart outcome that preserves the exact ID and distinguishes reattachment failure from task failure.

- [ ] **Step 1: Write failing end-to-end identity/restore tests**

Add a host test whose hand-derived assertions prove the stored value, not a fixture builder's output:

```rust
#[test]
fn restart_restore_uses_the_durable_provider_session_id_without_new_conversation_fallback() {
    let expected = ProviderSessionId::new("conversation-42").unwrap();
    let mut harness = provider_restore_harness_with_bound_session(expected.clone());

    let intent = harness.restart_restore_intent();

    assert_eq!(intent.provider_session_id.as_ref(), Some(&expected));
    assert_eq!(intent.mode, ProviderStartMode::ResumeExact);
}

#[test]
fn failed_exact_restore_does_not_mark_an_existing_task_failed() {
    let mut harness = provider_restore_harness_with_history("conversation-42");

    harness.complete_restore_with_error(ProviderStartError::ExactResumeUnavailable);

    assert_ne!(harness.task_attention(), Some(TaskAttention::Failed));
    assert_eq!(harness.persisted_history_len(), 2);
}
```

- [ ] **Step 2: Run the narrow tests and verify RED**

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-native-multi-task-VisualDevManager'
cargo test --locked --lib host::connection::tests::restart_restore_uses_the_durable_provider_session_id_without_new_conversation_fallback -- --exact --test-threads=1
cargo test --locked --lib host::connection::tests::failed_exact_restore_does_not_mark_an_existing_task_failed -- --exact --test-threads=1
```

Expected: the second test fails because `mark_provider_restore_failed` changes durable attention; the first exposes any lost/mismatched exact ID.

- [ ] **Step 3: Implement the minimal repair**

Retain the existing durable binding path. Replace restore-error promotion with a task-scoped runtime
attachment outcome. Keep `ResumeExact` when a durable ID exists; an unsupported or mismatched exact
resume becomes unavailable/detached and never `NewConversation`. Retire mismatched resource bindings
using existing fenced cleanup.

- [ ] **Step 4: Run both focused tests and verify GREEN**

Run the Task 1 commands; both must pass without touching the installed profile.

---

### Task 2: Cached-history visibility and monotonic conversation deltas

**Files:**
- Modify: `src/ui/task_workspace/surfaces.rs`
- Modify: `src/ui/task_cockpit/shell.rs`
- Modify: `src/ui/task_cockpit/timeline.rs`
- Modify: `src/ui/native_shell.rs`
- Modify only if required: `src/domain/cockpit.rs`, `src/domain/snapshot.rs`, `src/host/cockpit.rs`

**Interfaces:**
- Produces: `ConversationAdmission { page, changed }` (or an equivalently small existing-shape extension), monotonic requested/admitted high-water, and no-op admission for unchanged pages.

- [ ] **Step 1: Write failing cache and cockpit tests**

```rust
#[test]
fn unchanged_conversation_page_reports_no_change_and_keeps_the_same_high_water() {
    let task = TaskId::new();
    let mut registry = TaskSurfaceRegistry::default();
    registry.ensure_task(task);
    let first = registry.admit_conversation(task, 1, &page(1, "hello")).unwrap();
    let repeat = registry.admit_conversation(task, 2, &empty_page_after(1)).unwrap();

    assert!(first.changed);
    assert!(!repeat.changed);
    assert_eq!(repeat.page.high_water, 1);
}

#[test]
fn attachment_failure_keeps_persisted_conversation_rows_visible() {
    let mut shell = cockpit_with_conversation("saved answer");
    shell.set_attachment_unavailable("resume failed");

    let projection = shell.projection();

    assert!(projection.timeline_text().contains("saved answer"));
    assert!(projection.attachment_banner().contains("resume failed"));
}
```

- [ ] **Step 2: Verify RED with focused module tests**

```powershell
cargo test --locked --lib ui::task_workspace::surfaces::tests -- --test-threads=1
cargo test --locked --lib ui::task_cockpit::shell::tests -- --test-threads=1
```

- [ ] **Step 3: Implement monotonic delta admission and stable projection**

Use the existing exclusive `after_sequence` contract. Fast-path ordered appends, reject or ignore
stale generations, report `changed=false` for empty/duplicate pages, and reproject only the exact
changed task. Do not clone/sort all facts on every empty poll. Keep the persisted page installed in
the cockpit even when attachment state is unavailable.

- [ ] **Step 4: Make submission acknowledge locally before provider startup**

Add a pending user row on locally admitted send and reconcile it by stable identity when the journal
fact arrives. A command rejection marks the pending row failed instead of making the gesture appear
lost. Preserve all existing composer fences.

- [ ] **Step 5: Run Task 2 tests and verify GREEN**

Run both Task 2 commands and the existing native conversation cache tests in `native_shell`.

---

### Task 3: Honest terminal recovery and task activity

**Files:**
- Modify: `src/ui/task_workspace/surfaces.rs`
- Modify: `src/ui/native_shell.rs`
- Modify: `src/host/cockpit.rs` only if the existing result cannot express terminal unavailability

**Interfaces:**
- Produces: per-task terminal presentation state `Live`, `StaleReconnecting`, `Unavailable`, or `Exited`, retaining the last bounded admitted screen.

- [ ] **Step 1: Write failing terminal/status tests**

```rust
#[test]
fn restore_loss_keeps_last_terminal_screen_but_disables_input() {
    let mut surface = surface_with_terminal_lines(["build started", "compiling"]);
    surface.note_terminal_reconnecting();

    assert_eq!(surface.terminal_tail(8), vec!["build started", "compiling"]);
    assert!(!surface.terminal_is_interactive());
    assert_eq!(surface.terminal_label(), "Reconnecting — last terminal screen");
}
```

- [ ] **Step 2: Run the focused surface/native tests and verify RED**

```powershell
cargo test --locked --lib ui::task_workspace::surfaces::tests -- --test-threads=1
```

- [ ] **Step 3: Preserve the exact terminal snapshot and project accurate labels**

Keep real PTY output only. Do not clear the last screen on transient attachment loss. Permit input
only for the exact live resource/generation. Replace “Terminal is live; waiting for output” whenever
no live binding is proven.

- [ ] **Step 4: Wire provider/session activity to the existing visible status derivation**

Map current-generation active work to Working/Thinking, elicitation to Waiting, settled turn to
Idle, and actual runtime error to Failed. Keep TaskLifecycle::Settled as user-controlled Done.

- [ ] **Step 5: Run Task 3 tests and verify GREEN**

Run the focused surface and native-shell terminal/status tests.

---

### Task 4: Native notification sound

**Files:**
- Modify: `src/host/connection.rs`
- Modify if a defect is proven: `src/services/process_manager.rs`

**Interfaces:**
- Consumes: admitted `settings.notification_sound` and existing `ProcessManager::set_notification_sound`/`reconcile_ai_idle`.
- Produces: exactly one sound on a current-session Working-to-Idle completion.

- [ ] **Step 1: Write a failing configured-runtime test** proving the admitted sound ID reaches the native `ProcessManager`, plus a transition test proving replay/stale generations are silent.
- [ ] **Step 2: Run the exact tests and verify RED.**
- [ ] **Step 3: Apply the admitted setting during `ConfiguredServiceRuntime::initialized_from_admission` and reuse the existing v0.4.1 transition path.**
- [ ] **Step 4: Run the exact tests and verify GREEN.**

---

### Task 5: Compact dropdown selectors

**Files:**
- Modify: `src/ui/native_shell.rs`

**Interfaces:**
- Produces: explicit model/reasoning/access popover state and choice actions; removes the three click-cycle paths.

- [ ] **Step 1: Write failing GPUI projection/action tests** that open each selector, enumerate explicit choices, commit one choice, dismiss with Escape, and prove the persisted composer setting changed only to the clicked value.
- [ ] **Step 2: Run the exact native-shell tests and verify RED.**
- [ ] **Step 3: Reuse the repository's existing menu/popover recipe** with compact metrics, selected checkmark, keyboard focus, click-away dismissal, and accessible expanded/selected state. Delete `cycle_composer_model`, `cycle_composer_reasoning`, and `cycle_composer_access` after all callers move.
- [ ] **Step 4: Run the exact tests and verify GREEN.**

---

### Task 6: Rail scrolling, actions, and archived selection

**Files:**
- Modify: `src/ui/native_shell.rs`
- Modify only if needed: `src/ui/task_cockpit/inbox.rs`

**Interfaces:**
- Produces: corrected wheel offset, fixed scrollbar gutter, New task beside All projects, footer New project, no Search plus, and normal archived selection.

- [ ] **Step 1: Write failing behavior tests** that assert a downward wheel gesture advances toward later rows, row bounds do not intersect the scrollbar gutter, required control IDs appear exactly once in the intended locations, and an archived row commits selection without restoring it.
- [ ] **Step 2: Run the exact native-shell tests and verify RED.**
- [ ] **Step 3: Apply the minimal rail changes.** Reverse only the custom wheel delta, reserve an explicit narrow scrollbar column, reuse the existing new-task/new-project actions with corrected placement/icons, and route archived rows through the same navigation handler as other task IDs.
- [ ] **Step 4: Run the exact tests and verify GREEN.**

---

### Task 7: Frozen-diff verification and live acceptance

**Files:**
- Modify only production/test files required by Tasks 1–6.

- [ ] **Step 1: Review the complete diff against every spec acceptance criterion.** Remove unrelated edits and confirm `AGENTS.md` remains user-owned and unstaged.
- [ ] **Step 2: Run formatting and textual gates.**

```powershell
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 3: Run the complete serial library suite once.**

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-native-multi-task-VisualDevManager'
Remove-Item Env:DEVMANAGER_PROFILE -ErrorAction SilentlyContinue
cargo test --locked --lib -- --test-threads=1
```

- [ ] **Step 4: Run the all-target compiler gate once.**

```powershell
cargo check --locked --lib --bins --tests
```

- [ ] **Step 5: Perform the real watch-mode acceptance sweep.** Restart only the isolated dev app through its watcher, then click/type/focus through old and new Codex tasks, history after restart, prompt submission latency, streamed reply, terminal output, status transition and sound, dropdowns, rail scroll, New task/New project, settled, archived, multi-pane focus, and persistence.
- [ ] **Step 6: Verify cleanup and isolation.** Confirm no owned Cargo/rustc/linker/test harness remains; compare installed DevManager PID/start time and production `config.json`/`remote.json` hashes with the recorded baseline.
- [ ] **Step 7: Commit only the reviewed DevManager implementation and plan files.** Do not stage `AGENTS.md` and do not push.
