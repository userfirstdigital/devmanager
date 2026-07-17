# Browser Replay Executor Implementation Plan

> **For Codex:** Execute this plan in order with strict red-green-refactor cycles. Do not batch implementation ahead of its failing test.

**Goal:** Execute a compiled browser recipe sequentially through the existing `BrowserController`, with exact coordinator/cancellation fencing, portable tab aliases, typed waits/assertions, upload containment and approval risk, and value-free state/errors.

**Architecture:** Keep the checkpoint-7 compiler/coordinator in `replay.rs`; add a focused `replay_executor.rs` that owns setup, mapping, response proofs, and orchestration. Share the immutable replay plan with one non-debuggable execution handle. Drive tests through the real browser command channel and a fake inbox responder so production and tests use the same controller transport.

**Tech stack:** Rust 2021, Tokio, existing browser command/host/recipe/replay modules, integration tests, injected WebView initialization JavaScript.

---

### Task 1: Validate portable alias lifecycle and seed new recordings

**Files:**

- Modify: `tests/browser_replay.rs`
- Modify: `tests/browser_recording.rs`
- Modify: `src/browser/replay.rs`
- Modify: `src/browser/recording_coordinator.rs`
- Modify: `src/browser/host/windows.rs`

**Step 1: Write failing compiler tests**

Add exact tests proving:

- ordinary recipes may use active initial `tab-1`;
- a recipe that explicitly `CreateTab { tab: "tab-1" }` gets legacy mode and may use it only after creation;
- create introduces a previously unseen alias;
- select/close require an active alias;
- close removes the alias;
- duplicate or reused creation fails `BrowserReplayError::InvalidRecipe`.

**Step 2: Run the focused RED**

Run: `cargo test --test browser_replay replay_compile_rejects_invalid_tab_alias_lifecycle -- --exact`

Expected: FAIL because `compile_browser_replay` does not inspect alias lifecycle.

**Step 3: Implement the minimal compiler pass**

Add one pure bounded alias-lifecycle validator in `replay.rs`. Select legacy mode by the presence of a `CreateTab tab-1`, seed `tab-1` only otherwise, and reject references outside the active/seen sets. Return only `InvalidRecipe`.

**Step 4: Write the recording seed RED**

Add a recording coordinator test that starts with a selected runtime tab, records an action referencing it, and requires logical `tab-1`; prove later created tabs receive `tab-2` and ordinary `start` remains compatible.

Run: `cargo test --test browser_recording recording_start_seeds_selected_tab_as_tab_one -- --exact`

Expected: FAIL because recording start has no initial-tab seam.

**Step 5: Implement and verify the seed**

Add `start_with_selected_tab` (or an equivalently narrow seam), seed the alias table before recording commands can reserve, and make the Windows host pass the selected ID captured from the real workspace snapshot. Keep plain `start` as a compatibility wrapper.

Run both focused tests, then: `cargo test --test browser_replay --test browser_recording`

Expected: PASS.

### Task 2: Add the exact plan/cancellation execution handle

**Files:**

- Modify: `tests/browser_replay.rs`
- Modify: `src/browser/replay.rs`
- Modify: `src/browser/mod.rs`

**Step 1: Write failing handle tests**

Require one start-returned handle to be `Clone + Send + Sync`, neither `Debug` nor `Serialize`, share the exact cancellation authority, and retain access to the same immutable plan only through crate-private executor seams. Verify cancellation and replacement still invalidate every clone and terminal cleanup retains no value-bearing state.

Run: `cargo test --test browser_replay replay_execution_handle_shares_plan_and_cancellation_authority -- --exact`

Expected: FAIL because start currently returns only a cancellation lease and active state owns the plan directly.

**Step 2: Implement the minimal shared handle**

Move the compiled plan into one `Arc`, store only shared immutable ownership in active state and the execution handle, and preserve the public checkpoint-7 cancellation observations. Do not add value-bearing debug, serialization, projection, or error access.

**Step 3: Re-run focused and full replay tests**

Run: `cargo test --test browser_replay`

Expected: PASS.

### Task 3: Add typed host waits without raw JavaScript

**Files:**

- Modify: `tests/browser_host.rs`
- Modify: `tests/browser_recording.rs`
- Modify: `src/browser/automation.rs`
- Modify: `src/browser/host/initialization.rs`
- Modify: `src/browser/host/windows.rs`
- Modify: `src/browser/recording_coordinator.rs`

**Step 1: Write failing typed-wait tests**

Add serialization and injected-script tests for `NetworkIdle`, `Title`, `ElementAbsent`, and `ElementValue`. Prove title exact/contains, value exactness, absent semantics, bounded network-idle tracking, and that replay mapping never needs `JavaScript`.

Run: `cargo test --test browser_host typed_replay_waits_are_injected_without_javascript_predicates -- --exact`

Expected: FAIL because the variants and injected checks do not exist.

**Step 2: Implement typed variants and injection**

Extend `BrowserWaitCondition`, target-reference validation, exhaustive recording conversion, and the injected `checkWait`. Track in-flight fetch/XHR work and a bounded quiet window for network idle. Keep all existing wait behavior and host timeout bounds intact.

**Step 3: Verify host and recording compatibility**

Run: `cargo test --test browser_host typed_replay_waits_are_injected_without_javascript_predicates -- --exact`

Run: `cargo test --test browser_recording`

Expected: PASS.

### Task 4: Build setup and exact tab response fencing

**Files:**

- Create: `tests/browser_replay_executor.rs`
- Create: `src/browser/replay_executor.rs`
- Modify: `src/browser/mod.rs`
- Modify: `src/browser/commands.rs`

**Step 1: Write the fake-controller setup RED**

Use `browser_command_channel` and its real bound controller. Start execution and assert no request appears for a noncanonical/invalid root. For a valid root, require this exact awaited sequence:

1. `CreateTab { url: None }`
2. `UpdateViewport` on the returned selected runtime ID
3. `Navigate` to the compiled start URL

Respond one request at a time and prove a second request is not queued before the first response. Add wrong-response, missing/duplicate selected ID, cancellation, replacement, and late-response cases; none may mutate aliases or advance a step.

Run: `cargo test --test browser_replay_executor setup_uses_fresh_tab_and_awaits_each_exact_response -- --exact`

Expected: FAIL because the executor module does not exist.

**Step 2: Implement root preflight, context creation, and setup**

Add a narrow invocation-context constructor that creates a fresh operation ID for the supplied actor. Use fixed bounded intents. Begin the exact replay, preflight the authenticated canonical root, then issue setup commands sequentially through the existing controller. Collapse all raw host/root errors to fixed replay codes.

**Step 3: Implement bounded alias proofs**

Map setup to initial `tab-1` only outside legacy mode. For create/select/close, update local aliases solely after the exact returned snapshot proves the mutation. Require active aliases and ban ambient inference or alias reuse.

**Step 4: Verify setup/fencing tests**

Run: `cargo test --test browser_replay_executor setup_uses_fresh_tab_and_awaits_each_exact_response -- --exact`

Run the cancellation/replacement tests in the same target.

Expected: PASS.

### Task 5: Map every action and file risk sequentially

**Files:**

- Modify: `tests/browser_replay_executor.rs`
- Modify: `src/browser/replay_executor.rs`

**Step 1: Write failing action table tests**

Cover every `BrowserRecipeAction` and exact response type. Require one-action `Act` batches; semantic Download as Click; validated CdpMarker method with `{}`; action-level Wait; and exact selected tab evolution. Assert every context has the supplied actor, a unique operation ID, fixed bounded value-free intent, and Normal declared risk except classified uploads.

Run: `cargo test --test browser_replay_executor every_recipe_action_maps_to_one_existing_command -- --exact`

Expected: FAIL on the first unmapped action.

**Step 2: Implement the minimal action compiler**

Resolve literals/public inputs only at command construction time. Convert recipe locators to semantic action targets. Require exact command/response pairs and never format a value into executor errors or intents.

**Step 3: Write file containment/risk REDs**

Test relative in-root files, absolute in-root files, missing files, outside-root files, and escaping symlinks where supported. Require canonical command paths, `Normal` versus `OutsideWorkspaceFile`, the authenticated-root request path, and no path in replay error/status/debug output.

Run: `cargo test --test browser_replay_executor upload_resolves_at_execution_and_declares_containment_risk -- --exact`

Expected: FAIL until upload resolution is complete.

**Step 4: Implement upload resolution and verify**

Join relative values to the verified root, preserve absolute candidates, call `classify_upload_path`, and send only the canonical result through `request_with_local_project_root`.

Run: `cargo test --test browser_replay_executor every_recipe_action_maps_to_one_existing_command -- --exact`

Run: `cargo test --test browser_replay_executor upload_resolves_at_execution_and_declares_containment_risk -- --exact`

Expected: PASS.

### Task 6: Execute waits/assertions and terminal transitions

**Files:**

- Modify: `tests/browser_replay_executor.rs`
- Modify: `src/browser/replay_executor.rs`

**Step 1: Write failing ordering and assertion tests**

Prove action, optional wait, and assertions run in declared order; every assertion maps to a short bounded typed wait; the coordinator advances only after the final assertion; and the next step begins only afterward. Add `matched: false`, transport error, response mismatch, cancellation between calls, replacement while awaiting, and late-response tests.

Run: `cargo test --test browser_replay_executor replay_runs_action_wait_assertions_and_advances_only_after_success -- --exact`

Expected: FAIL because orchestration is incomplete.

**Step 2: Implement orchestration and safe failure collapse**

Poll cancellation/exact status before and after every await and before every transition. Map ordinary wait false/errors to `StepFailed`, assertion false to `AssertionFailed`, interruption to Cancelled/stale, stop on first failure, and complete only after all steps advance. Return only safe projections or closed replay errors.

**Step 3: Verify the executor target**

Run: `cargo test --test browser_replay_executor`

Expected: PASS.

### Task 7: Platform, documentation, and final gates

**Files:**

- Modify: `docs/browser-automation.md`
- Modify if required: `src/browser/host/unsupported.rs`
- Modify if required: exact tests affected by exhaustive wait variants

**Step 1: Add public architecture/runbook documentation**

Document fresh-tab setup, portable `tab-1`, sequential execution, typed waits, cancellation fencing, outside-workspace upload approval, and the explicit checkpoint exclusions.

**Step 2: Run focused and aggregate browser gates**

Run:

- `cargo test --test browser_replay --test browser_replay_executor --test browser_recording --test browser_recipes --test browser_host`
- `cargo test browser`
- `cargo check`
- `cargo build --release`
- `cargo fmt --all -- --check`
- `git diff --check c57cfd6f0c1c80caf00f2439550a15655ea7c12e..HEAD`

If an Apple target is installed, also run `cargo check --target x86_64-apple-darwin`; otherwise use the existing unsupported-host compile/test surface and report the unavailable cross-target honestly.

Expected: all available gates PASS with no warnings introduced by checkpoint 8.

**Step 3: Commit and package the exact review artifact**

Commit coherent implementation slices, then produce `.superpowers/sdd/review-c57cfd6..<head>.diff`, its stable patch ID, and SHA-256. Confirm a clean worktree and request an independent review of only `c57cfd6..<head>` before claiming completion.
