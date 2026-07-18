# Browser Replay Locator Repair Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development and strict red-green-refactor cycles. Complete checkpoint 10 and obtain independent approval before starting checkpoint 11.

**Goal:** Pause an exact replay on a typed locator failure with pinned evidence, then preview and atomically save a current-revision replacement and optionally retry the same step.

**Architecture:** Extend the existing `BrowserReplayCoordinator`; do not create another lifecycle owner. Keep the executor alive across repair with a value-free watch signal and private locator overrides. Reuse the controller queue, resources, approval policy, hardened recipe store, and journal.

**Approved base:** `f9f1657b04cff4153c0402dbfb38a7d57a632e34`

## Global constraints

- Checkpoint 10 must be frozen and independently approved before checkpoint 11 production edits.
- No `browser_workflow` MCP schema, provider lifecycle bridge, or whole-PC work.
- No selector, page text, path, input value, secret, or arbitrary callback message in repair projections/errors/journals.
- One active repair per active replay; exact opaque instance fencing on every read or mutation.
- Evidence is owner-scoped and retained atomically by one exact repair lease; a process-global per-canonical-root runtime prevents cleanup races and reconciles crash-stale repair pins.
- Cancellation and apply share one coordinator linearization gate: cancellation wins before any write, or apply commits file+override+`applied` state coherently before cancellation proceeds.
- Resume is phase-aware and must not repeat a successful mutating action after a later wait or assertion failure.
- Every task starts with a focused failing test and ends in a coherent commit.

---

## Checkpoint 10 — typed failure, evidence, and stable pause

### Task 1: Typed host locator failure

**Files:**

- Modify: `src/browser/model.rs`
- Modify: `src/browser/host/initialization.rs`
- Modify: `src/browser/host/windows.rs`
- Modify: `src/browser/mcp.rs`
- Modify: `tests/browser_host.rs`

1. Write failing Windows/Node tests for fixed `Primary`, `Source`, and `Destination` missing-target codes. Prove arbitrary exception text still collapses to `CrashedView`, while secret target disappearance/change is fixed `Primary`.
2. Run the exact tests and capture RED.
3. Add `BrowserLocatorFailureTarget` and `BrowserError::LocatorNotFound`. Update injected fixed-code mapping, secret completion, display, unsupported behavior, and MCP error-code conversion.
4. Prove serialized/Debug errors contain no locator or page sentinel and ordinary crash errors remain distinct.
5. Run `browser_host` and `browser_replay_secrets`; commit `feat(browser): add typed locator failures`.

### Task 2: Exact repair state and pin lease

**Files:**

- Create: `src/browser/replay_repair.rs`
- Modify: `src/browser/replay.rs`
- Modify: `src/browser/commands.rs`
- Modify: `src/browser/resources.rs`
- Modify: `src/browser/host/windows.rs`
- Modify: `src/browser/mod.rs`
- Create: `tests/browser_replay_repair.rs`
- Modify: `tests/browser_replay.rs`

1. Write failing trait/identity/state tests for `BrowserReplayLocatorSlot`, opaque `BrowserReplayRepairInstance`, safe projection phases, one repair per replay, stale/cross-workspace/cross-coordinator calls, checked IDs, and terminal immutability.
2. Write failing real-store tests for a shared process-global canonical-root gate/runtime, exact lease/owner/kind validation, write+retain before cleanup, second-capture rollback, release retry, retention while paused, unpin on cancel/replace/terminal/drop, and next-process reconciliation of stale dedicated repair pins.
3. Add a private repair-retention sidecar to controller envelopes. Only exact Agent replay snapshot/screenshot captures may use it, and the Windows host must store them as dedicated repair kinds through `put_repair_retained`; ordinary MCP captures cannot mint or retain repair evidence.
4. Add one private repair state to `ActiveBrowserReplay`, one non-clone evidence retention lease, one private override map, one phase-aware resume cursor, and a value-free Tokio watch generation shared with the execution handle. Remove the unrestricted placeholder resume transition; only a confirmed apply may resume.
5. Keep repair values, leases, and sidecars non-Debug/non-serde; projection contains only IDs, slot, revision, phase, tab ID, and handles.
6. Run replay/repair/resources/host/command tests; commit `feat(browser): add exact replay repair state`.

### Task 3: Executor evidence capture and paused wait

**Files:**

- Modify: `src/browser/replay_executor.rs`
- Modify: `tests/browser_replay_executor.rs`
- Modify: `tests/browser_replay_repair.rs`

1. Write failing fake-controller tests where an action returns each typed missing-target kind and where eligible element waits/assertions reach page-condition timeout.
2. Require this exact sequence after failure: create exact retention lease, semantic snapshot retained before cleanup, validate, viewport screenshot retained before cleanup, validate, exact coordinator pause. No later recipe command may be issued while paused.
3. Implement locator-slot mapping and resume cursors for primary/optional/source/destination/action-wait/step-wait/assertion. Preserve absent/hidden semantics that intentionally succeed without an element.
4. Keep `execute_browser_replay` alive on the watch receiver. Prove cancel, replace, and workspace interruption wake it, return the retained terminal projection, close secrets, ignore late responses, and release evidence. Prove a nested-wait or assertion repair resumes without replaying the already successful action. Capture/retention failure remains `StepFailed` with whole-lease rollback.
5. Run replay executor, repair, secret, host, coordinator, and resource suites; commit `feat(browser): pause replay with locator evidence`.

### Task 4: Checkpoint-10 evidence and independent review

**Files:**

- Modify: `.superpowers/sdd/browser-task-5c-checkpoints.md`
- Modify: `.superpowers/sdd/browser-task-5c-report.md`
- Modify: `.superpowers/sdd/progress.md`

1. Run focused suites plus `cargo test --locked browser -- --test-threads=1 -j 1`, `cargo check --locked --all-targets -j 1`, format, and exact diff checks.
2. Audit repair projections, errors, resources, journals, Debug, serde, and secret-store lifetime for value leakage.
3. Commit `docs(browser): record locator repair pause evidence`.
4. Freeze a path-scoped `f9f1657..HEAD` artifact with byte size, SHA-256, raw stable patch ID, and byte-identical regeneration. Stop for independent review. Do not begin preview/apply until APPROVED.

---

## Checkpoint 11 — preview, atomic apply, and same-step resume

### Task 5: Current-revision highlight-only preview

**Files:**

- Modify: `src/browser/commands.rs`
- Modify: `src/browser/automation.rs`
- Modify: `src/browser/host/initialization.rs`
- Modify: `src/browser/host/windows.rs`
- Modify: `src/browser/host/unsupported.rs`
- Modify: `src/browser/replay_repair.rs`
- Modify: `tests/browser_host.rs`
- Modify: `tests/browser_replay_repair.rs`

1. Write failing Node/host tests for a pointer-transparent owned highlight with an exact generation token that dispatches no focus/click/input events, is excluded from DOM revision tracking, and uses compare-and-swap install/clear so an old completion cannot overwrite or clear a newer overlay.
2. Write failing authority tests for exact repair instance, candidate `BrowserElementRef` revision, semantic locator validation, User/Agent actors, late callbacks, old-completion/new-preview ordering, navigation, and changed page/workspace/replay/repair.
3. Add internal token-bearing highlight/clear commands through the existing queue and journal, explicitly excluded from recording. Store a candidate only after exact host acknowledgement and post-await coordinator compare-and-swap; recheck cancellation and native revision after the script callback.
4. Run host/repair/recording tests; commit `feat(browser): preview replay locator repairs`.

### Task 6: Exact-step atomic recipe replacement

**Files:**

- Modify: `src/browser/recipes.rs`
- Modify: `src/browser/replay.rs`
- Modify: `src/browser/replay_repair.rs`
- Modify: `tests/browser_recipes.rs`
- Modify: `tests/browser_replay_repair.rs`

1. Write failing tests for canonical recipe digest, exact step index+ID+slot+old-locator comparison, every locator slot, changed recipe, invalid candidate, exact-once replay binding to the authenticated canonical root, reparse boundaries, concurrent apply, injected replacement failure, and temp cleanup.
2. Store the validated canonical digest privately in the replay plan and bind the authenticated canonical root exactly once in the execution handle before the first command. Add locator-at/replace-at helpers and reuse the existing global `RECIPE_WRITE_GATE`; do not add an independent repair gate.
3. Reload and compare, clone and replace only the exact locator, validate the full v1 recipe, recompare at the final boundary, and use the existing atomic sibling replacement. Preserve the old complete file on every failure.
4. Run recipe/repair tests; commit `feat(browser): atomically save locator repairs`.

### Task 7: Explicit confirmation, approval, and same-step resume

**Files:**

- Modify: `src/browser/commands.rs`
- Modify: `src/browser/host/windows.rs`
- Modify: `src/browser/host/unsupported.rs`
- Modify: `src/browser/replay.rs`
- Modify: `src/browser/replay_executor.rs`
- Modify: `src/browser/replay_repair.rs`
- Modify: `tests/browser_host.rs`
- Modify: `tests/browser_replay_executor.rs`
- Modify: `tests/browser_replay_repair.rs`

1. Write failing tests requiring preview plus explicit confirmation, a `Destructive` Agent approval floor, higher-risk preservation, denial/interruption/stale approval fencing, and no repository write before authorization.
2. Write deterministic race tests proving cancellation/replace/interrupt wins before commit with no write, or apply wins through file+override+`applied` state before terminalization. Add a post-write/pre-resume page-revision change test: preserve the new complete recipe, remain `applied`, and issue no browser action.
3. Write failing executor tests for exact override slot, no progress advance before retry, action/action-wait/step-wait/assertion resume cursors, no duplicate successful mutation, applied-without-resume, later exact resume after a fresh preview, successful phase completion, second repair ID on another failure, and cleanup.
4. Add one exact `Preparing`/`Committing` apply reservation under the coordinator gate. After the final pre-write host validation, synchronously commit file+override+`applied` state under that gate. Then post-validate page/token/revision before an optional resume. Never mutate the immutable plan or another locator slot.
5. Run all replay/repair/host/recording/secret suites; commit `feat(browser): resume repaired replay steps`.

### Task 8: Checkpoint-11 evidence and independent review

**Files:**

- Modify: `docs/browser-automation.md`
- Modify: `.superpowers/sdd/browser-task-5c-checkpoints.md`
- Modify: `.superpowers/sdd/browser-task-5c-report.md`
- Modify: `.superpowers/sdd/progress.md`

1. Document repair evidence, preview, approval, atomic update, resume, cleanup, and the explicit checkpoint-12 MCP/lifecycle exclusions.
2. Run focused suites, aggregate browser single-job, locked all-target check single-job, Windows release build with the installed `GPUI_FXC_PATH`, format, and exact diff checks.
3. Audit changed-recipe/page/workspace races, old-or-new atomic file guarantees, resource pins, journal safety, unsupported/macOS compilation, and secret survival/zeroization.
4. Commit `docs(browser): record locator repair completion`, freeze the exact checkpoint-11 artifact, and stop for independent review before checkpoint 12.
