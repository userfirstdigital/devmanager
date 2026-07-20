# Browser Workflow MCP and Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development and strict red-green-refactor cycles. Freeze each task commit before independent review, and keep the existing `BrowserReplayCoordinator` as the sole replay owner.

**Goal:** Expose saved browser workflows through one exact bearer-bound `browser_workflow` MCP tool, execute them through the existing controller/approval/journal path, surface secret and locator-repair handoffs in the native companion pane, and synchronously cancel replay at every browser, provider, and application lifecycle boundary.

**Architecture:** Store one `BrowserReplayCoordinator` inside the existing `BrowserCommandBridge` runtime shared by the GPUI shell and MCP gateway. MCP prepares a replay and spawns the existing executor, while the executor waits on the coordinator's existing value-free signal if native secret entry is required. MCP and native repair controls resolve opaque replay/repair identities through that same coordinator and call the existing controller preview/apply lanes. Browser controls, direct input, route changes, provider registration revocation, process loss, and shutdown all enter one bridge cancellation function before host work continues.

**Tech Stack:** Rust 2021, Tokio/watch and the existing bounded command channel, rmcp 2.2 Streamable HTTP, serde/schemars, GPUI, Wry/WebView2 on Windows, the hardened recipe/resource stores, and the existing Windows/unsupported host adapters.

## Global Constraints

- Checkpoint 12 only. Do not add whole-PC control, Playwright, Node sidecars, external Chrome, desktop-control MCP tools, or a second browser/replay owner.
- The MCP operation enum is exactly `list|get|replay|status|cancel|repairPreview|repairApply`; no route, project, conversation, profile, token, secret, password, cookie, raw path, or file-content field may be accepted.
- Every MCP operation requires a bounded nonblank `intent` and the existing exact risk enum. Preview preserves the declared context; apply combines the declared risk with the existing Destructive repair floor and approval policy.
- Public replay inputs are only `text|url|file`. Secret input names may appear only in value-free projections; secret values never enter MCP, resources, logs, diagnostics, journal entries, serde/debug state, or provider environment.
- `BrowserReplayCoordinator` remains the sole authority for active/terminal status, replacement, cancellation, secret submission, repair identity, preview/apply state, and resume.
- Recipe `get` returns an owner-scoped MCP resource handle. Recipe bytes, repair evidence, screenshots, and other large payloads never appear inline.
- Stop, direct input, logical-tab close, conversation switch/close, workspace reset, project-profile clear, registration replacement/revocation, provider process loss, gateway loss, and app shutdown cancel synchronously before late work can advance or write.
- Windows functionality must remain native and macOS must compile through the existing unsupported adapter with a clear `unavailable_platform` result. Browser failure must never prevent the Claude/Codex terminal from launching or continuing.
- Every production task starts with a focused failing test, records RED, implements the smallest coherent GREEN, reruns the focused and aggregate gates with `-j 1`, and ends in a separate commit.

---

### Task 1: Put replay readiness and lifecycle cancellation on the shared bridge

**Files:**

- Modify: `src/browser/replay.rs`
- Modify: `src/browser/replay_executor.rs`
- Modify: `src/browser/commands.rs`
- Modify: `src/browser/mod.rs`
- Modify: `tests/browser_replay.rs`
- Modify: `tests/browser_replay_executor.rs`
- Modify: `tests/browser_host.rs`
- Create: `tests/browser_workflow_lifecycle.rs`

**Interfaces:**

```rust
pub struct BrowserReplayActiveState {
    pub instance: BrowserReplayInstance,
    pub projection: BrowserReplayProjection,
    pub repair_instance: Option<BrowserReplayRepairInstance>,
    pub repair: Option<BrowserReplayRepairProjection>,
}

impl BrowserReplayCoordinator {
    pub fn active_state(&self, workspace: &BrowserWorkspaceKey)
        -> Option<BrowserReplayActiveState>;
    pub fn exact_instance(&self, workspace: &BrowserWorkspaceKey, instance_id: u64)
        -> Result<BrowserReplayInstance, BrowserReplayError>;
    pub fn exact_repair(&self, workspace: &BrowserWorkspaceKey, instance_id: u64, repair_id: u64)
        -> Result<BrowserReplayRepairInstance, BrowserReplayError>;
    pub fn interrupt_project(&self, project_id: &str) -> usize;
    pub fn interrupt_all(&self) -> usize;
}

impl BrowserCommandBridge {
    pub fn replay_coordinator(&self) -> BrowserReplayCoordinator;
    pub fn interrupt_all(&self);
}
```

The active-state carrier is cloneable but not serde. It contains only existing value-free public projections plus opaque non-serde identities. Exact lookup compares workspace, numeric identity, and the coordinator-private scope; callers cannot fabricate an active authority.

- [ ] **Step 1: Write failing coordinator and bridge lifecycle tests**

Add tests covering exact active/terminal/stale lookup; secret readiness wake; project/all interruption; and one replay shared by bridge, inbox, and controller. Exercise every `BrowserHostControl` variant, `BrowserHostEvent::UserInput`, `BrowserCommand::Stop`, `CloseTab`, `ResetWorkspace`, and `ClearProjectProfile`. Assert cancellation is installed before a queued/late response and that a tab-scoped interrupt conservatively cancels the owning workspace replay.

- [ ] **Step 2: Run and capture RED**

Run:

```powershell
cargo test --locked -j 1 --test browser_workflow_lifecycle -- --test-threads=1
cargo test --locked -j 1 --test browser_replay_executor secret -- --test-threads=1
```

Expected: compile failures for the absent bridge coordinator/accessors and a failing executor case because `NeedsUserSecret` is not yet awaitable.

- [ ] **Step 3: Add exact replay lookup and reuse the value-free watch signal for readiness**

Signal the existing replay watch when secrets are submitted as well as on repair/terminal transitions. At executor entry, verify the controller/root/actor first, then wait while the exact projection is `NeedsUserSecret`. Continue only when the same instance becomes `Pending` or `Running`; return the retained terminal projection on cancellation/replacement. No polling loop and no additional cancellation authority are allowed.

- [ ] **Step 4: Make the bridge the one cancellation fan-out**

Create the coordinator once in `browser_command_channel` and clone it into `BrowserCommandBridge`, `BrowserCommandInbox`, and bound `BrowserController` values. Under the existing host-control lock, apply each lifecycle event to both cancellation epochs and the replay coordinator. `InterruptTab` cancels the owning workspace replay because one recipe may use several runtime tabs. Preserve existing host-control ordering and registration-lease fencing.

- [ ] **Step 5: Run focused and aggregate GREEN**

Run:

```powershell
cargo test --locked -j 1 --test browser_workflow_lifecycle -- --test-threads=1
cargo test --locked -j 1 --test browser_replay -- --test-threads=1
cargo test --locked -j 1 --test browser_replay_executor -- --test-threads=1
cargo test --locked -j 1 --test browser_host -- --test-threads=1
cargo test --locked -j 1 browser -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 6: Commit**

Commit message: `feat(browser): unify replay lifecycle cancellation`

---

### Task 2: Add a platform-neutral workflow service and exact seven-operation MCP schema

**Files:**

- Create: `src/browser/workflow_mcp.rs`
- Modify: `src/browser/mcp.rs`
- Modify: `src/browser/resources.rs`
- Modify: `src/browser/gateway.rs`
- Modify: `src/browser/mod.rs`
- Create: `tests/browser_workflow_mcp.rs`
- Modify: `tests/browser_gateway.rs`

**Wire interface:**

```rust
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserWorkflowRequestWire {
    #[schemars(length(max = 1024))]
    intent: String,
    risk: BrowserMcpRisk,
    operation: BrowserWorkflowOperation,
    recipe_id: Option<String>,
    inputs: Option<Vec<BrowserWorkflowPublicInputWire>>,
    replay_instance_id: Option<u64>,
    repair_id: Option<u64>,
    candidate: Option<BrowserElementRef>,
    confirm: Option<bool>,
    resume: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
enum BrowserWorkflowOperation {
    List,
    Get,
    Replay,
    Status,
    Cancel,
    RepairPreview,
    RepairApply,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserWorkflowPublicInputWire {
    name: String,
    kind: BrowserWorkflowPublicInputKind, // text | url | file only
    value: String,
}
```

Operation contracts:

- `list`: accepts no operation fields and returns sorted compact recipe metadata, input names/kinds, viewport, and step count without defaults or paths.
- `get`: requires only `recipeId`; validates and stores canonical pretty v1 bytes as `BrowserResourceKind::WorkflowRecipe`, then returns compact metadata plus its owner-scoped handle.
- `replay`: requires `recipeId`, accepts bounded `inputs`, rejects every Secret kind/value, validates through `compile_browser_replay`, replaces any exact active replay, spawns `execute_browser_replay` with the authenticated controller/root/store, and returns the value-free projection.
- `status`: requires only `replayInstanceId`; returns the exact retained projection and, while paused, the exact value-free repair projection/resource handles.
- `cancel`: requires only `replayInstanceId`; synchronously cancels that exact instance and returns the terminal projection.
- `repairPreview`: requires `replayInstanceId`, `repairId`, and `candidate`; creates a private candidate only after exact lookup, preserves the MCP intent/risk context through the existing queue/journal, and returns the repair projection.
- `repairApply`: requires `replayInstanceId`, `repairId`, `confirm: true`, and explicit `resume`; uses the existing Destructive-floor approval/write/validation lane and returns repair, replay, and `recipeWritten`.

Every operation rejects fields belonging to another operation. `status`, `cancel`, and repair operations never infer a foreign or latest route; the bearer-bound workspace plus exact numeric identities are both required.

- [ ] **Step 1: Write failing schema, malformed-wire, resource, and operation tests**

Assert exactly one `browser_workflow` tool, the exact operation/risk/public-input enums, `additionalProperties: false`, bounded intent/input fields, and the absence of route/token/secret/path fields. Cover every missing, irrelevant, duplicate, oversized, zero, stale, cross-workspace, and unknown argument. Assert malformed calls return structured typed errors without ending the authenticated MCP session.

Use real repository fixtures to test sorted `list`, owner-isolated `get` resources, unknown future schema rejection, input compilation, no-default required inputs, and Secret rejection. Drive a minimal real replay through the authenticated gateway and existing fake host queue to completion; then verify exact `status`, replacement, `cancel`, and late-response fencing. Exercise preview/apply helpers against a real coordinator-paused repair in module tests, including stale replay/repair IDs, page revision drift, confirmation, approval denial, recipe drift, write result, and resume.

- [ ] **Step 2: Run and capture RED**

Run:

```powershell
cargo test --locked -j 1 --test browser_workflow_mcp -- --test-threads=1
cargo test --locked -j 1 --test browser_gateway browser_workflow -- --test-threads=1
```

Expected: failures because the workflow module, resource kind, tool schema, and dispatch do not exist.

- [ ] **Step 3: Implement contained repository/resource operations**

Reuse `verified_authenticated_local_project_root`, `list_recipes`, `load_recipe`, canonical recipe serialization, and `BrowserResourceStore`. Map filesystem/recipe errors to fixed path-free tool messages. Add `WorkflowRecipe` to resource kind handling and cleanup tests; do not pin recipe resources or duplicate repository files into app data permanently.

- [ ] **Step 4: Implement exact replay/status/cancel dispatch**

Construct the invocation context for every operation before work. Call `validate_and_ensure` so the first workflow use opens the companion pane. Clone the controller/coordinator/store/root into one spawned executor future. If the executor returns a nonterminal error, terminalize the exact instance as failed; if cancellation/replacement already won, retain that terminal result. Do not retain a second task-owned status registry.

- [ ] **Step 5: Wire preview/apply through existing private controller lanes**

Add a context-preserving preview entry point beside the existing actor wrapper. Resolve opaque instances only from the coordinator, validate the candidate's current revision in the existing host path, and return value-free projections. Apply must reuse `request_replay_repair_apply`; no direct recipe write, approval bypass, or synthetic resume signal is permitted.

- [ ] **Step 6: Run focused and aggregate GREEN**

Run:

```powershell
cargo test --locked -j 1 --test browser_workflow_mcp -- --test-threads=1
cargo test --locked -j 1 --test browser_gateway browser_workflow -- --test-threads=1
cargo test --locked -j 1 --test browser_gateway -- --test-threads=1
cargo test --locked -j 1 --test browser_replay_executor -- --test-threads=1
cargo test --locked -j 1 --test browser_replay_repair -- --test-threads=1
cargo test --locked -j 1 browser -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 7: Commit**

Commit message: `feat(browser): expose workflow replay through mcp`

---

### Task 3: Surface replay, secret, and locator-repair state in the native companion pane

**Files:**

- Modify: `src/browser/pane.rs`
- Modify: `src/app/mod.rs`
- Modify: `src/browser/host/windows.rs`
- Modify: `src/browser/host/unsupported.rs`
- Modify: `tests/browser_secret_prompt.rs`
- Modify: `tests/browser_host.rs`
- Create: `tests/browser_workflow_pane.rs`

**Interfaces:**

```rust
pub struct BrowserReplayPaneProjection {
    pub replay: BrowserReplayProjection,
    pub repair: Option<BrowserReplayRepairProjection>,
    pub selecting_replacement: bool,
}

pub enum BrowserPaneAction {
    // existing variants
    CancelReplay { instance_id: u64 },
    BeginReplayRepairSelection { instance_id: u64, repair_id: u64 },
    ApplyReplayRepair { instance_id: u64, repair_id: u64, resume: bool },
}
```

The pane shows only recipe ID, status, current/total step, unresolved Secret names, repair step/slot/phase, and evidence availability. It never renders a selector, candidate locator, default/input value, local path, secret value, or recipe body.

- [ ] **Step 1: Write failing pane-model/action and secret handoff tests**

Cover Running, NeedsUserSecret, PausedLocatorRepair, and terminal disappearance; exact-instance Cancel; automatic one-time masked prompt installation; submit-to-coordinator wake; prompt Escape cancellation; replacement; route switch; and stale submission. Assert the page surface is hidden while entering secrets but visible while selecting a repair candidate.

Add rendering/source tests for the compact replay status, `Cancel replay`, `Select replacement`, `Save repair`, and `Save and retry` controls at the 320px minimum pane width. The model/debug output must remain value-free.

- [ ] **Step 2: Run and capture RED**

Run:

```powershell
cargo test --locked -j 1 --test browser_workflow_pane -- --test-threads=1
cargo test --locked -j 1 --test browser_secret_prompt -- --test-threads=1
```

Expected: compile failures for absent pane projection/actions and failing prompt installation because the launch boundary is not wired.

- [ ] **Step 3: Install and retire the native secret prompt from coordinator state**

During the existing 33ms GPUI browser pump, inspect only the active workspace replay. Install the existing fixed-capacity `BrowserReplaySecretPromptVault` once for an exact `NeedsUserSecret` instance. Its submitter closure captures the coordinator and exact opaque instance and calls `submit_secrets`; Escape calls exact replay cancellation before dropping the vault. Replacement, route change, revocation, and terminal state consume and zeroize stale prompt state. Keep all plaintext in the existing NativeShell-owned zeroizing vault.

- [ ] **Step 4: Add native replay status and cancellation**

Project `BrowserCommandBridge::replay_coordinator().active_state` into `BrowserPaneTransient`. Render a compact status block above the page/journal and wire Cancel to the exact coordinator instance. The existing global Stop command remains authoritative and, through Task 1, cancels the same replay.

- [ ] **Step 5: Reuse annotation selection only as a repair candidate picker**

When the exact paused repair enters selection mode, start the existing annotation overlay on its exact tab. That overlay already suppresses ordinary user-input events while active. Intercept its semantic element candidate before annotation screenshot/draft capture, immediately cancel the annotation route to clear accepted-candidate state, convert only revision/locator to `BrowserElementRef` with no backend ID, and asynchronously call the context-preserving User preview lane. Normal annotation behavior is unchanged outside exact repair-selection state. Region candidates are rejected; route/replay/repair/revision changes clear selection without preview.

- [ ] **Step 6: Apply or apply-and-resume through the existing approval lane**

`Save repair` supplies explicit confirmation with `resume: false`; `Save and retry` supplies explicit confirmation with `resume: true`. Both use a User invocation context and the existing repair apply controller. Update only compact pane status on completion. Recipe writes stay in the coordinator/store gate and host approval remains active.

- [ ] **Step 7: Run focused and aggregate GREEN**

Run:

```powershell
cargo test --locked -j 1 --test browser_workflow_pane -- --test-threads=1
cargo test --locked -j 1 --test browser_secret_prompt -- --test-threads=1
cargo test --locked -j 1 --test browser_host -- --test-threads=1
cargo test --locked -j 1 --test browser_replay_repair -- --test-threads=1
cargo test --locked -j 1 browser -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 8: Commit**

Commit message: `feat(browser): add native replay repair controls`

---

### Task 4: Close provider, conversation, profile, and application lifecycle gaps

**Files:**

- Modify: `src/browser/gateway.rs`
- Modify: `src/services/process_manager.rs`
- Modify: `src/app/mod.rs`
- Modify: `tests/browser_gateway.rs`
- Modify: `tests/browser_host.rs`
- Modify: `tests/browser_workflow_lifecycle.rs`
- Modify: `tests/process_manager.rs` if present; otherwise add focused module tests beside existing process-manager browser registration tests

**Lifecycle matrix:**

| Event | Required synchronous action |
|---|---|
| Browser Stop with/without tab | cancel owning workspace replay before host stop |
| Direct trusted page input | cancel owning workspace replay before page revision advances |
| Logical browser tab close | cancel owning workspace replay before close dispatch |
| Conversation selection changes | cancel previous workspace replay and zeroize its prompt before hiding its views |
| Conversation close/delete/import removal | interrupt exact workspace before process/session teardown |
| Reset workspace | cancel workspace replay before reset |
| Clear project profile/delete project | cancel every project replay before profile/project mutation |
| Registration replacement/revoke/process exit | revoke lease and cancel exact workspace before removing registration |
| Gateway failure/drop | cancel all registered workspace replays before service teardown |
| App quit/update/force quit | interrupt all replay work before scheduling process shutdown or exiting |

- [ ] **Step 1: Write failing lifecycle matrix and provider-loss tests**

Use an active replay and retained late controller response for each boundary. Assert terminal `Cancelled`, closed secret leases, released repair evidence, no later step advancement/write, and no cross-workspace cancellation. Test a reused process/session ID so an old late process-exit callback cannot cancel a replacement registration's newer replay except through its exact registration lease.

- [ ] **Step 2: Run and capture RED**

Run:

```powershell
cargo test --locked -j 1 --test browser_workflow_lifecycle -- --test-threads=1
cargo test --locked -j 1 browser_provider -- --test-threads=1
```

Expected: route-switch, close, and shutdown assertions fail until NativeShell calls the shared bridge boundary.

- [ ] **Step 3: Wire NativeShell route/close/reset/profile/shutdown boundaries**

Call the shared bridge before discarding workflow UI, closing an AI tab, removing project-owned tabs, resetting state, clearing profiles, scheduling app/update shutdown, or direct exit. Do not add lifecycle logic inside general process spawning. Remote Server/SSH paths remain browser-free.

- [ ] **Step 4: Verify existing provider cleanup reaches the shared bridge**

Keep registration ownership in `BrowserGatewayRegistrar`. Reuse its exact `revoke`, `revoke_process`, `revoke_all`, replacement, and gateway-drop paths; prove each calls the bridge combined cancellation before removal. Preserve the current provider fallback: if registration or injection fails, launch the unmodified Claude/Codex terminal and retain a browser diagnostic.

- [ ] **Step 5: Add unsupported/macOS compile and behavior checks**

The unsupported host must reject replay host commands with `UnavailablePlatform`, expose no functional pane controls, and contain no Wry/WebView2 imports. The platform-neutral MCP/workflow modules must compile without Windows cfg assumptions. Keep existing macOS ARM64 CI authoritative; run the local all-target check on Windows.

- [ ] **Step 6: Run focused and aggregate GREEN**

Run:

```powershell
cargo test --locked -j 1 --test browser_workflow_lifecycle -- --test-threads=1
cargo test --locked -j 1 --test browser_gateway -- --test-threads=1
cargo test --locked -j 1 --test browser_host -- --test-threads=1
cargo test --locked -j 1 browser_provider -- --test-threads=1
cargo test --locked -j 1 browser -- --test-threads=1
cargo check --locked --all-targets -j 1
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 7: Commit**

Commit message: `fix(browser): cancel replay across native lifecycle`

---

### Task 5: Document, freeze, review, and release-verify checkpoint 12

**Files:**

- Modify: `docs/browser-automation.md`
- Modify: `.superpowers/sdd/browser-task-5c-checkpoints.md`
- Modify: `.superpowers/sdd/browser-task-5c-report.md`
- Modify: `.superpowers/sdd/progress.md`

- [ ] **Step 1: Document the final contract**

Document the exact MCP operations/fields/results, owner/root binding, value-free resources, secret handoff, native repair flow, cancellation matrix, provider fallback, Windows support, and macOS unsupported behavior. Remove checkpoint-12 pending language without claiming whole-PC control.

- [ ] **Step 2: Run the complete locked verification suite**

Run:

```powershell
cargo fmt --all -- --check
cargo test --locked --all-targets -j 1 -- --test-threads=1
cargo check --locked --all-targets -j 1
$env:GPUI_FXC_PATH='C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64\fxc.exe'; cargo build --locked --release -j 1
git diff --check
git status --short
```

Run explicit scans proving no secret wire, no route override, no second replay owner, no Playwright/Node/external Chrome, and no Windows imports in the unsupported/platform-neutral modules. Record counts and command outputs in the checkpoint report.

- [ ] **Step 3: Freeze and independently review the immutable range**

Commit documentation as `docs(browser): record workflow lifecycle completion`. Freeze the exact checkpoint-12 base-to-head binary diff, record byte count, SHA-256, and raw `git patch-id --stable`, verify byte-identical regeneration and reverse apply, then obtain a fresh spec/quality review with P0-P3 severity. Fix and re-review every finding before acceptance.

- [ ] **Step 4: Run Windows package/release gates**

Run the repository's existing Windows x64 packaging path and verify the produced executable/package metadata. Confirm release CI definitions still cover Windows x64, Windows ARM64, and macOS ARM64 without adding a partial macOS browser implementation.

- [ ] **Step 5: Perform real installed-app acceptance with computer use**

Launch the newly built DevManager without disturbing the user's currently installed session until the test binary is ready. In real Claude and Codex conversations, verify token-isolated tools can list/get/replay/status/cancel a repository workflow; the visible split pane follows only its owning conversation; secret entry is masked and resumes; direct input and Stop cancel; a local fixture locator failure can be selected, previewed, saved, and retried; upload/download and evidence resources remain owner-scoped; process restart cancels stale replay and reinjects a rotated token; and terminal use survives forced gateway/browser unavailability.

- [ ] **Step 6: Final clean-tree handoff**

Report exact commits, verification evidence, release artifact paths, manual acceptance results, known Windows-only boundary, and the separate future whole-PC control seam. Do not merge, push, or replace the installed app unless the user explicitly asks.
