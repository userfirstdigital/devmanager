# Browser Replay Memory-Only Secrets Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a paused browser replay receive secrets from a masked DevManager-only prompt and type them through a zeroizing private host lane without exposing plaintext to serializable/debuggable browser state.

**Architecture:** One exact-instance `BrowserReplaySecretStore` is shared by the replay coordinator and execution handle. A value-free `SecretType` command uses an unforgeable private envelope sidecar to reach the existing Agent queue, target inspection, approval, recording, and journal seams; the pane receives only names and masked-set state.

**Tech Stack:** Rust, `zeroize`, Tokio channels, existing BrowserController/Windows Wry host, GPUI pane model, serde/static assertions, Node-backed injected-script tests.

## Global Constraints

- Checkpoint 9 only: do not add `browser_workflow`, locator repair, recipe writes, or a second lifecycle owner.
- Secret plaintext never enters `BrowserAction`, serialized `BrowserCommand` fields, invocation context, MCP, status, errors, resources, approval requests, recorder state, diagnostics, or journal entries.
- Secret submission/editor/store types are bounded and non-`Debug`, non-serde; submissions and editor vaults are non-`Clone`.
- Terminalization synchronously closes and zeroizes the shared store; retained host leases cannot expose after closure.
- Only Agent replay execution may use the secure host lane.
- Every task follows RED-to-GREEN TDD and ends in a coherent commit.

---

### Task 1: Exact-instance zeroizing secret store

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `src/browser/replay_secrets.rs`
- Modify: `src/browser/mod.rs`
- Modify: `src/browser/replay.rs`
- Create: `tests/browser_replay_secrets.rs`

**Interfaces:**
- Produces `BrowserReplaySecretSubmission::from_user_prompt(Vec<(String, String)>)` as a crate-private consuming constructor.
- Produces `BrowserReplaySecretStore`, `BrowserReplaySecretLease`, and closed `BrowserReplaySecretError`.
- Extends `BrowserReplayExecutionHandle::secret_lease(&str)` and `BrowserReplayCoordinator::submit_secrets`.

- [ ] **Step 1: Write failing trait, submission, and terminal-clear tests**

Add tests using `static_assertions::assert_not_impl_any!` for `Debug`, `Serialize`, `DeserializeOwned`, and `Clone` on submission/editor-facing value containers. Cover exact required names, empty/oversized/duplicate/missing/extra values, second submission, stale instance, foreign coordinator/workspace, and fixed non-echoing errors. Add a test-only zeroization observer that proves a retained lease cannot expose after complete/fail/cancel/replace/interrupt and that the owned bytes were zeroized on close.

- [ ] **Step 2: Run the focused target and capture RED**

Run: `cargo test --locked --test browser_replay_secrets -- --test-threads=1`

Expected: compile failure for absent secret types/APIs.

- [ ] **Step 3: Implement the minimal store and coordinator integration**

Add direct `zeroize = { version = "1.8", features = ["derive"] }`. Store values as `Zeroizing<String>` behind one mutex-protected exact-instance authority. Submission validates all names before moving any value into the store. `submit_secrets` accepts only `NeedsUserSecret`, atomically installs once, transitions to `Running`, and clears only the safe unresolved-name projection. `terminalize` and coordinator drop call idempotent `close()` before releasing active state.

- [ ] **Step 4: Run focused replay tests**

Run:

```powershell
cargo test --locked --test browser_replay_secrets -- --test-threads=1
cargo test --locked --test browser_replay -- --test-threads=1
```

Expected: both targets pass with no sentinel in formatted/serialized projections or errors.

- [ ] **Step 5: Commit**

Commit message: `feat(browser): add memory-only replay secret store`

### Task 2: Private secure controller sidecar

**Files:**
- Modify: `src/browser/commands.rs`
- Modify: `src/browser/automation.rs`
- Modify: `src/browser/mod.rs`
- Modify: `tests/browser_host.rs`
- Modify: `tests/browser_replay_secrets.rs`

**Interfaces:**
- Adds value-free `BrowserCommand::SecretType { tab_id, target, input_name }`.
- Adds crate-private `BrowserController::request_replay_secret_type(command, context, lease)`.
- Extends private `BrowserCommandEnvelope`/`BrowserCommandRequest` with `Option<BrowserReplaySecretLease>` plus exact `validate_secret_sidecar()`.

- [ ] **Step 1: Write failing forgery and wire-safety tests**

Prove ordinary `request_with_context(SecretType, ...)` reaches the host with no sidecar and is rejected before execution; a sidecar attached to another command/input/workspace is rejected; only the secure method creates the exact marker/lease pair. Serialize and Debug every public marker/context/status and assert the sentinel is absent. Prove cancellation tickets and registration revocation drop the pending secure request.

- [ ] **Step 2: Run and capture RED**

Run: `cargo test --locked --test browser_replay_secrets secure_command -- --test-threads=1`

Expected: compile failure for the absent marker and secure request method.

- [ ] **Step 3: Implement the sidecar without plaintext command fields**

Add the safe marker to operation-name/tab-id/lifecycle/automation classification. Add the optional lease only to private envelope/request structs. Normal send methods always set it to `None`; the secure method validates Agent context, exact controller workspace and marker name against the lease before enqueue. Host-facing validation returns fixed `InvalidExecutionAuthority`/`InvalidInvocation` errors and never exposes the secret.

- [ ] **Step 4: Run focused command/host tests**

Run:

```powershell
cargo test --locked --test browser_replay_secrets secure_command -- --test-threads=1
cargo test --locked --test browser_host -- --test-threads=1
```

Expected: secure-lane and existing 91+ host tests pass.

- [ ] **Step 5: Commit**

Commit message: `feat(browser): add private secret command lane`

### Task 3: Windows queue, approval, recording, and injected typing

**Files:**
- Modify: `src/browser/host/initialization.rs`
- Modify: `src/browser/host/windows.rs`
- Modify: `src/browser/host/unsupported.rs`
- Modify: `src/browser/recording_coordinator.rs`
- Modify: `tests/browser_host.rs`
- Modify: `tests/browser_workflow_coordinator.rs`
- Modify: `tests/browser_replay_secrets.rs`

**Interfaces:**
- Adds injected `window.__devmanagerBrowser.typeSecret(target, value)` returning only `completedActions`.
- Adds secure inspect/approval/action async phases that retain the sealed lease, never plaintext.
- Recording maps the safe marker to an unset Secret input.

- [ ] **Step 1: Write failing real-queue and Node harness tests**

Drive the real command inbox/Windows host test seams through target inspection, AccountSecurity approval, approve/deny, Stop, direct input, registration revocation, and late callback. Add a Node harness that types a sentinel, observes input/change events, and returns `{completedActions:1}` while console/network/page IPC output omits the sentinel. Recording tests must show only an unset Secret input and safe `Type` reference.

- [ ] **Step 2: Run and capture RED**

Run:

```powershell
cargo test --locked --test browser_replay_secrets windows_secure -- --test-threads=1
cargo test --locked --test browser_workflow_coordinator secret_type -- --test-threads=1
```

Expected: failures for absent host phases/injected function/recording mapping.

- [ ] **Step 3: Implement secure host execution**

Queue `SecretType` only for Agent requests. Reserve the value-free command for recording, inspect its target, calculate existing runtime risk, and await existing approval. After approval, call `lease.with_exposed` to build `Zeroizing<String>` JSON and outer script buffers and invoke `typeSecret`; do not store either buffer on async phase state. Return the standard value-free action response. Denial/interruption/crash/route loss drops the request and lease. Unsupported host returns `UnavailablePlatform` without exposure.

- [ ] **Step 4: Run host/recording/redaction suites**

Run:

```powershell
cargo test --locked --test browser_replay_secrets -- --test-threads=1
cargo test --locked --test browser_host -- --test-threads=1
cargo test --locked --test browser_workflow_coordinator -- --test-threads=1
cargo test --locked --test browser_recording -- --test-threads=1
```

Expected: all pass; sentinel scan is clean outside the test's controlled DOM assertion.

- [ ] **Step 5: Commit**

Commit message: `feat(browser): type replay secrets through approved host lane`

### Task 4: Executor integration and masked pane contract

**Files:**
- Modify: `src/browser/replay_executor.rs`
- Modify: `src/browser/pane.rs`
- Modify: `src/app/mod.rs`
- Modify: `src/browser/mod.rs`
- Modify: `tests/browser_replay_executor.rs`
- Create: `tests/browser_secret_prompt.rs`

**Interfaces:**
- Executor routes only Secret-kind recipe `Type` inputs through `request_replay_secret_type`.
- Adds safe `BrowserReplaySecretPromptEvent` and `BrowserReplaySecretPromptProjection`.
- Adds a non-cloneable `BrowserReplaySecretPromptVault`; pane model receives only names, focus, and `is_set`.

- [ ] **Step 1: Write failing executor and pane-safety tests**

Compile a recipe with Text and Secret type actions. Assert Text emits ordinary `BrowserAction::Type`; Secret emits only the value-free `SecretType` marker plus private sidecar. Cancel between inspect/approval/action and verify Cancelled plus closed store. Test prompt install/edit/backspace/submit/cancel/route-switch with fixed-mask rendering and assert the sentinel is absent from pane model Debug, event JSON, snapshots, remote snapshots, and journal/resources.

- [ ] **Step 2: Run and capture RED**

Run:

```powershell
cargo test --locked --test browser_replay_executor secret -- --test-threads=1
cargo test --locked --test browser_secret_prompt -- --test-threads=1
```

Expected: compile failures for missing executor secure routing and prompt types.

- [ ] **Step 3: Implement executor routing and prompt projection**

Expose plan input kind without exposing values. For Secret-kind `Type`, acquire the exact lease and await the secure controller request; require the standard one-action response. Close the store on every terminal executor return. Keep the prompt vault in NativeShell-owned volatile memory outside `BrowserPaneTransient`; derive the pane projection from names and booleans only. Render a constant eight-bullet mask for every set value. Submit consumes the vault into `BrowserReplaySecretSubmission`; all dismiss/route changes close it.

- [ ] **Step 4: Run executor/pane/app suites**

Run:

```powershell
cargo test --locked --test browser_replay_executor -- --test-threads=1
cargo test --locked --test browser_secret_prompt -- --test-threads=1
cargo test --locked browser_pane -- --test-threads=1
```

Expected: all pass with no sentinel in safe surfaces.

- [ ] **Step 5: Commit**

Commit message: `feat(browser): add masked replay secret prompt contract`

### Task 5: Checkpoint verification and immutable review package

**Files:**
- Modify: `.superpowers/sdd/browser-task-5c-checkpoints.md`
- Modify: `.superpowers/sdd/browser-task-5c-report.md`
- Modify: `.superpowers/sdd/progress.md`

**Interfaces:**
- Produces an immutable checkpoint-9 commit range and evidence package; does not start checkpoint 10.

- [ ] **Step 1: Run focused and aggregate gates**

Run:

```powershell
cargo test --locked --test browser_replay_secrets -- --test-threads=1
cargo test --locked --test browser_secret_prompt -- --test-threads=1
cargo test --locked --test browser_replay_executor -- --test-threads=1
cargo test --locked --test browser_host -- --test-threads=1
cargo test --locked --test browser_workflow_coordinator -- --test-threads=1
cargo test --locked --test browser_recording -- --test-threads=1
cargo test --locked browser -- --test-threads=1
cargo check --locked --all-targets
cargo build --release --locked
cargo fmt --all -- --check
git diff --check
```

Expected: every command exits 0.

- [ ] **Step 2: Run explicit leakage scans**

Run the sentinel-bearing integration tests, then inspect serialized events, MCP tool schemas, resource fixtures, journals, recording JSON, Debug captures, and repository output. The sentinel may appear only in test source and the controlled page DOM assertion.

- [ ] **Step 3: Update checkpoint evidence and commit**

Document exact RED/GREEN commands, counts, platform limits, and explicit checkpoint 10-12 exclusions. Commit message: `docs(browser): record memory-only secret evidence`.

- [ ] **Step 4: Freeze and review**

Create `.superpowers/sdd/review-<base>..<head>.diff`, record byte size, SHA-256, and stable patch ID, verify a clean worktree, and stop for independent spec/quality review. Do not start locator repair until APPROVED.
