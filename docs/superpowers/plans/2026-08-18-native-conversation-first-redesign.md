# Native Conversation-First Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rebuild DevManager's native task conversation into a calm Codex-style surface with a rich multiline composer, real native image attachments, useful semantic activity, and a default-collapsed context dock.

**Architecture:** Keep the sealed semantic journal and durable provider-input path authoritative. Improve the GPUI presentation at the timeline/native-shell layer; stage image bytes locally through a reusable bounded helper and send only provider-readable path references through the existing fenced provider command. Migrate the local layout schema once so the context dock starts collapsed without discarding the user's other geometry.

**Tech Stack:** Rust, GPUI, AccessKit, serde, image, existing DevManager host/client and semantic journal contracts.

**Spec:** `docs/superpowers/specs/2026-08-18-native-conversation-first-redesign.md`

## Global constraints

- Work in the current dirty `master` checkout because the new native shell foundation is uncommitted here; preserve every unrelated edit.
- No commits, merges, destructive Git operations, dependency additions, protocol identity inference, or raw-PTY conversation derivation.
- Tests use an isolated `CARGO_TARGET_DIR` beneath `C:\Temp\devmanager-conversation-redesign` and full library tests run serially.
- Do not change installed DevManager state. Watch-mode reload may stop only its exact isolated live app/host paths.
- Run focused RED/GREEN checks during implementation and broad gates only after the diff is frozen.

---

### Task 1: Present a semantic conversation instead of debug rows

**Files:**
- Modify: `src/ui/task_cockpit/timeline.rs`
- Modify: `src/ui/renderers/journal_view.rs` only if presentation classification needs a sealed helper
- Test: `src/ui/task_cockpit/timeline.rs`
- Test: `tests/ui_native_shell.rs`

**Interfaces:**
- Consumes: `TimelineItemModel`, `TimelineItemContent`, `ThemeTokens`, `SemanticJournalView`
- Produces: `conversation_item_visibility(&TimelineItemModel) -> ConversationItemVisibility`, rich `Timeline::surface`, and useful-only `activity_summary`

- [ ] **Step 1: Write failing behavior tests**

Add real projection tests proving that lifecycle/usage extension items are suppressed, user and assistant messages retain their content/role classification, and an idle activity summary does not contain a raw event count. The mutation caught is restoring the current every-event debug-row mapping.

- [ ] **Step 2: Run RED**

Run:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-conversation-redesign'
cargo test --locked --lib ui::task_cockpit::timeline::tests -- --test-threads=1
```

Expected: the new visibility/activity assertions fail because all items and the event count are currently rendered.

- [ ] **Step 3: Implement the minimum semantic presentation model**

Introduce a small classification enum:

```rust
enum ConversationItemVisibility {
    Message,
    Reasoning,
    Activity,
    Callout,
    HiddenChrome,
}
```

Classify by `TimelineItemContent`, with generic lifecycle/usage extensions hidden. Render message, tool, plan, question, approval, error, and fallback content through separate GPUI builders. Use an 860-pixel centered column, no per-row full-width border, and no `max_h(280)` cap.

- [ ] **Step 4: Run GREEN**

Run the Task 1 command again and require all focused tests to pass.

---

### Task 2: Build the floating multiline native composer

**Files:**
- Modify: `src/ui/native_shell.rs`
- Modify: `src/ui/task_cockpit/shell.rs`
- Modify: `src/ui/task_cockpit/composer.rs` only for presentation-independent attachment/draft accessors
- Test: `tests/ui_native_shell.rs`

**Interfaces:**
- Consumes: existing `TaskComposer`, `ComposerIntent`, slash catalog, question/approval projections, `ElementInputHandler`
- Produces: one centered interactive composer footer, wrapped draft presentation, attachment/action row, and unchanged fenced dispatch behavior

- [ ] **Step 1: Write failing interaction/render tests**

Add headless native-shell assertions for a composer capped at the conversation measure, Shift+Enter preserving a newline, Enter invoking the existing send path, a visible attachment control, and a collapsed-dock Tools affordance. The mutations caught are returning to the flat full-width footer or losing keyboard behavior.

- [ ] **Step 2: Run RED**

Run:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-conversation-redesign'
cargo test --locked --test ui_native_shell native_conversation -- --test-threads=1
```

Expected: new structure/accessibility assertions fail against the current flat composer.

- [ ] **Step 3: Implement the minimum composer layout**

Keep `native-task-composer-input` and `native-task-composer-send`. Replace the full-width bordered footer with a centered wrapper and raised rounded card, render multiline draft text without truncating to 256 characters, place slash suggestions immediately above it, and keep all existing answer/approval/provider-terminal listeners wired to the same `TaskComposer`.

- [ ] **Step 4: Run GREEN**

Run the Task 2 command again and require all focused tests to pass.

---

### Task 3: Add bounded native image paste and file attachment

**Files:**
- Modify: `src/remote/web/image_paste.rs`
- Modify: `src/ui/native_shell.rs`
- Test: `src/remote/web/image_paste.rs`
- Test: `tests/ui_native_shell.rs`

**Interfaces:**
- Consumes: `RemoteImageAttachment`, selected task `host_binding().workspace_root()`, GPUI `ClipboardEntry::Image`, `PathPromptOptions`
- Produces: reusable `stage_image_for_workspace`, task/agent-scoped `NativeComposerImage`, paste/file admission, removal, preview rendering, and submission text references

- [ ] **Step 1: Write failing staging and ownership tests**

Add tests proving a valid PNG/JPEG stages beneath `.devmanager/pasted-images`, produces an `@.devmanager/pasted-images/...` reference, and removal cleans an unsent staged file. Add native-shell tests proving an image clipboard payload is admitted for the selected task, invalid/oversize data leaves the draft intact with an inline error, and switching tasks does not leak attachment state.

- [ ] **Step 2: Run RED**

Run:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-conversation-redesign'
cargo test --locked --lib native_composer_image -- --test-threads=1
```

Expected: helper and native attachment tests fail because the native composer accepts text only.

- [ ] **Step 3: Implement reusable staging and native admission**

Refactor the existing web helper so both web and native paths call the same validation, sanitized filename, hidden directory, and TTL cleanup implementation. Store only bounded bytes/preview/path metadata in the native pending state; never add image bytes to `ProviderInputAction` or durable events. Read file bytes in a GPUI background task and apply results only if the exact Task/Agent composer key is still current.

- [ ] **Step 4: Inject references at the provider boundary**

For send/steer/queue actions, prefix the copied `ComposerIntent` text with the current attachment references before `to_provider_input_request`. Preserve the original draft/submission snapshot for retry. Clear native attachments only when the existing exact command settles successfully; preserve them on rejection/failure.

- [ ] **Step 5: Run GREEN**

Run the Task 3 command again and require all focused tests to pass.

---

### Task 4: Make conversation-first layout the durable default

**Files:**
- Modify: `src/ui/workspace_layout.rs`
- Modify: `src/ui/native_shell.rs`
- Test: `src/ui/workspace_layout.rs`
- Test: `tests/ui_native_shell.rs`

**Interfaces:**
- Consumes: v1 `LayoutFile`, `WorkspaceLayout::toggle(PaneEdge::Dock)`, existing Dock header control
- Produces: v2 layout persistence and one-time v1 migration with `dock_collapsed = true`

- [ ] **Step 1: Write the failing migration test**

Write a literal v1 layout fixture with custom widths, window frame, selected task, and open dock. Assert load preserves every value except the dock is collapsed. The mutation caught is either losing user geometry or keeping the competing dock open.

- [ ] **Step 2: Run RED**

Run:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-conversation-redesign'
cargo test --locked --lib ui::workspace_layout::tests -- --test-threads=1
```

Expected: the new migration assertion fails because only v1 is accepted and its open-dock value is retained.

- [ ] **Step 3: Implement the migration and low-chrome conversation panel**

Write v2 on save, accept v1 only through the explicit migration branch, and leave foreign schemas fail-closed. Keep the existing Dock toggle in the header. Render the center conversation as a low-chrome canvas while retaining bordered/shadowed inspector panels.

- [ ] **Step 4: Run GREEN**

Run the Task 4 command again and require all focused tests to pass.

---

### Task 5: Full review, correction batch, and final verification

**Files:**
- Review every file changed by Tasks 1-4 plus the pre-existing related diffs
- Update focused tests only when review exposes a real uncovered behavior

**Interfaces:**
- Consumes: frozen implementation diff
- Produces: one corrected, formatted, compile-clean, runtime-proven native conversation redesign

- [ ] **Step 1: Review the complete diff once**

Check provider identity/fence preservation, task switching, async file-picker staleness, image bounds, draft settlement, hidden lifecycle rows, accessibility identities, persisted layout migration, and unrelated user edits. Batch all corrections into one pass.

- [ ] **Step 2: Freeze and format only edited Rust files**

Run `cargo fmt --all`, inspect formatter changes, and revert no unrelated user work.

- [ ] **Step 3: Run final gates once**

Run:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-conversation-redesign'
cargo check --locked --lib --bins --tests
cargo test --locked --lib -- --test-threads=1
cargo test --locked --test provider_input
cargo test --locked --test ui_native_shell
git diff --check
```

- [ ] **Step 4: Run live watch-mode acceptance**

Restart only the isolated watch app/host. Verify first task row latency, open a real Claude/Codex task, inspect the conversation visually, paste a PNG, send a prompt referencing it, exercise `/`, Shift+Enter, question response, Dock reopen/collapse, and provider-terminal return. Confirm the installed app PIDs/start times and config/remote hashes did not change.

