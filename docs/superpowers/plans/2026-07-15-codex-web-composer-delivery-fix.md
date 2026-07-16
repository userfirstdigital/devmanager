# Codex Web Composer Delivery Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every AI native-web prompt and slash command reach the provider while showing terminal-only Codex status results.

**Architecture:** Keep prompt construction and PTY delivery in `execute_web_composer_batch`, using Codex's deterministic preflight and one neutral trailing-space slash-autocomplete dismissal. Close known Claude provider interactions at the web view's return-to-native boundary, where that state is authoritative. Remove the obsolete one-shot lifecycle recovery, then reuse the existing acknowledged provider-command handoff for `/status`.

**Tech Stack:** Rust, Windows ConPTY, React, TypeScript, Vitest, Vite

## Global Constraints

- Apply one preflight Escape before every submitted Codex batch and retain a post-text Escape only for ordinary Codex prompts.
- Close known Claude provider interactions when returning to native mode; do not send speculative Escape keys from a fresh Claude composer.
- Close Claude and Codex slash autocomplete with one harmless trailing space and a 500 ms PTY settle before Enter; exact no-argument Claude commands use a second Enter to execute the accepted entry.
- Keep non-AI submitted batches as prompt then carriage return.
- Do not add retries, resume controls, or direct provider-protocol submission.
- Preserve attachment rollback, writer-lease validation, and acknowledgement ordering.
- Manually test the hot-reload build at a phone-sized viewport before merging.

---

### Task 1: Deterministic Codex PTY submission

**Files:**
- Modify: `src/remote/web/image_paste.rs`
- Modify: `src/services/process_manager.rs`
- Test: `src/remote/web/image_paste.rs`
- Test: `src/services/process_manager.rs`

**Interfaces:**
- Consumes: `SessionRuntimeState.session_kind: SessionKind` and the existing `write(&str) -> Result<(), String>` callback.
- Produces: ordinary Codex batches write `["\u{1b}", prompt, "\u{1b}", "\r"]`; ordinary Claude batches write `[prompt, "\r"]`; AI slash batches append one trailing space before carriage return. Returning from a known Claude provider interaction writes Escape before the next composer mutation can be sent.

- [ ] **Step 1: Write the failing delivery tests**

Add tests that call `execute_web_composer_batch` twice for a Codex session and assert both calls write prompt, Escape, carriage return. Add a draft-only Codex test that asserts a prompt without trailing carriage return performs one write. Retain the Claude attachment test's existing prompt and carriage-return assertion.

- [ ] **Step 2: Run the focused Rust test and verify RED**

Run: `cargo test remote::web::image_paste::tests -- --test-threads=1`

Expected: the new Codex test fails because each current call writes only prompt and carriage return.

- [ ] **Step 3: Implement the minimal provider-specific sequence**

For submitted Codex batches, write one preflight `"\u{1b}"` and wait 180 ms. Type the leading slash-command token at 100 ms per character and bulk-write any arguments; bulk-write ordinary prompts unchanged. For Claude and Codex slash commands, wait 250 ms, write one trailing space, wait 500 ms for the ConPTY queue, then write `"\r"`. For exact Claude commands without arguments, wait 180 ms and write a second `"\r"`; argument-bearing commands submit once. For ordinary Codex prompts, wait 50 ms, write Escape, wait 120 ms, and write carriage return. For ordinary Claude prompts, wait 50 ms and write carriage return directly. When returning from a labeled Claude provider interaction, send Escape through the ordered raw-input lane before clearing the label. Leave non-submitted drafts unchanged.

- [ ] **Step 4: Remove the obsolete one-shot recovery**

Delete `initial_composer_escape_claimed`, `claim_initial_composer_escape`, and `ProcessManager::claim_codex_initial_composer_escape`. Remove the delayed recovery block from `handle_web_composer_batch`. Retain provider-turn observation because the semantic adapter uses it independently.

- [ ] **Step 5: Run the focused Rust test and verify GREEN**

Run: `cargo test remote::web::image_paste::tests -- --test-threads=1`

Expected: all image-paste and composer-batch tests pass.

- [ ] **Step 6: Commit the Rust fix**

Run: `git add src/remote/web/image_paste.rs src/services/process_manager.rs && git commit -m "fix(web): deliver every Codex composer submission"`

### Task 2: Codex terminal-only command handoff

**Files:**
- Modify: `web/src/sessions/commands/builtinCatalog.ts`
- Test: `web/src/sessions/commands/commandCatalog.test.ts`
- Test: `web/src/sessions/Composer.test.tsx`

**Interfaces:**
- Consumes: built-in `SlashCommand.interaction` metadata and `Composer.onProviderCommandSubmitted`.
- Produces: exact Codex `/status` submissions invoke provider-view handoff only after `onSubmit` resolves.

- [ ] **Step 1: Write failing catalog and composer tests**

Assert Codex `/status` has interaction `providerMenu`. Add a composer test that submits `/status`, verifies no handoff before acknowledgement, resolves the acknowledgement, and verifies the matching command is handed off.

- [ ] **Step 2: Run focused web tests and verify RED**

Run: `npm test -- --run web/src/sessions/commands/commandCatalog.test.ts web/src/sessions/Composer.test.tsx`

Expected: `/status` remains `inline`, so both new expectations fail.

- [ ] **Step 3: Classify Codex `/status` as a provider interaction**

Set the Codex `/status` seed's `interaction` to `"providerMenu"`. Do not alter argument handling or unrelated command classifications.

- [ ] **Step 4: Run focused web tests and verify GREEN**

Run: `npm test -- --run web/src/sessions/commands/commandCatalog.test.ts web/src/sessions/Composer.test.tsx`

Expected: both files pass with no warnings or unhandled errors.

- [ ] **Step 5: Commit the web behavior**

Run: `git add web/src/sessions/commands/builtinCatalog.ts web/src/sessions/commands/commandCatalog.test.ts web/src/sessions/Composer.test.tsx && git commit -m "fix(web): show Codex status in provider view"`

### Task 3: Build and automated verification

**Files:**
- Modify: `web/bundle/**` generated by the production web build, if tracked output changes.

**Interfaces:**
- Consumes: the completed Rust and TypeScript fixes.
- Produces: a deployable embedded web bundle and green repository gates.

- [ ] **Step 1: Format and run focused regression tests**

Run: `cargo fmt --check`

Run: `cargo test remote::web::image_paste::tests -- --test-threads=1`

Run: `npm test -- --run web/src/sessions/commands/commandCatalog.test.ts web/src/sessions/Composer.test.tsx`

Expected: all commands succeed.

- [ ] **Step 2: Run all web tests and the production build**

Run: `npm test -- --run`

Run: `npm run build`

Expected: all Vitest tests pass and Vite refreshes the tracked embedded bundle.

- [ ] **Step 3: Run the full Rust suite**

Run: `cargo test -- --test-threads=1`

Expected: all Rust unit and integration tests pass.

- [ ] **Step 4: Commit generated bundle changes**

Run: `git add web/bundle && git commit -m "build(web): refresh Codex composer bundle"`

### Task 4: Manual hot-reload acceptance

**Files:**
- No source changes expected.

**Interfaces:**
- Consumes: the isolated worktree build and real local Claude/Codex installations.
- Produces: browser evidence that provider interactions work end to end.

- [ ] **Step 1: Start the isolated hot-reload profile**

Run: `powershell -ExecutionPolicy Bypass -File .\dev-watch.ps1 -Once`

Expected: the development manager starts on its isolated remote-web port and serves the new bundle.

- [ ] **Step 2: Test consecutive Codex prompts at 390 x 844**

Open a Codex native session, submit `Reply with DM_FIRST_OK only.`, wait for `DM_FIRST_OK`, then submit `Reply with DM_SECOND_OK only.` and wait for `DM_SECOND_OK` without opening the raw terminal or pressing any resume control.

- [ ] **Step 3: Test Codex slash interactions**

Submit exact `/model` and verify the real Codex model selector opens after acknowledgement. Return to native, submit exact `/status`, and verify the real status pane opens with model, permissions, usage, and context details.

- [ ] **Step 4: Test Claude compatibility**

Submit a Claude prompt and confirm either a provider response or a real provider error reaches the session. Submit exact `/model` and verify the Claude selector opens.

- [ ] **Step 5: Restore browser and stop the hot-reload process**

Return the user's live tab to its original session, close or release temporary test tabs, finalize browser control, and stop only the isolated hot-reload process.

### Task 5: Integrate and verify release

**Files:**
- No source changes expected beyond automated release-version commits.

**Interfaces:**
- Consumes: verified feature branch commits.
- Produces: updated `master`, pushed origin, and a successful GitHub release workflow.

- [ ] **Step 1: Re-run final verification on the feature branch**

Run: `git status --short`, `cargo fmt --check`, `npm test -- --run`, `npm run build`, and `cargo test -- --test-threads=1`.

Expected: clean working tree and all gates pass.

- [ ] **Step 2: Merge into current master**

Fetch origin, fast-forward local `master`, then fast-forward merge `codex/fix-codex-composer-delivery`. Re-run the same final gates from `master`.

- [ ] **Step 3: Push master and monitor GitHub Actions**

Run: `git push origin master`.

Expected: the release workflow succeeds and publishes the next release with a Windows x64 updater manifest pointing to that version.

- [ ] **Step 4: Clean the isolated worktree**

Remove `.worktrees/fix-codex-composer-delivery` and delete the merged local feature branch after the workflow is confirmed.
