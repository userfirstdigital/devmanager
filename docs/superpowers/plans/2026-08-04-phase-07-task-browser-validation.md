# Phase 7: Task Browser and LLM Validation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a real WebView2 browser a Task-owned host resource that follows the task across desktop clients, exposes a permissioned browser tool surface to stock providers, supplies the bounded projection consumed by later Connect clients, survives presentation changes, and is proven end to end rather than declared complete from mocks or compilation.

**Architecture:** The durable host owns browser context/tab identities, WebView2 environments/profiles, operation queues, automation, MCP lifecycle, downloads, screenshots, recordings, recipes, and teardown. On Windows the actual page is an OS child surface; a first vertical-slice proof must demonstrate that a host-owned WebView2 child HWND can be safely attached to, hidden from, and reattached to the GPUI context dock across processes. Clients receive browser state/screenshot/progress projections; provider tools call a Task/generation-scoped kernel gateway, never a free-floating helper.

**Tech Stack:** Rust, Wry 0.55.1/WebView2 COM 0.38.2, GPUI/raw-window-handle, RMCP, existing browser automation/recording/replay modules, deterministic local fixture site, stock provider CLIs.

## Global Constraints

- Do not refactor the complete browser subsystem until the cross-process attach/focus/DPI/teardown proof passes on the isolated binary.
- The browser host is Rust-owned and explicit in ProcessOwner/Command Center. No Node/Playwright sidecar becomes the runtime browser harness.
- Every browser operation carries TaskId, BrowserContextId, tab ID, runtime generation, request ID, permission, and cancellation identity.
- Browser contexts use isolated WebView2 user-data directories beneath the active profile. Production browser data is never copied, opened, migrated, or shared.
- Provider browser tools are Task-scoped and expose only capabilities granted to that Task; no global “current browser” lookup.
- Page input requires an active browser focus epoch and bounds generation. Task-switch pointer gestures are consumed and never forwarded into page content.
- Desktop and a transport-neutral projection client observe the same tabs/navigation/automation state in this phase. Phase 8 carries that exact projection through direct and hosted Connect; no client creates a second browser.
- Raw page content, screenshots, downloads, recordings, secrets, and clipboard data remain local/E2E-only by default.
- Browser completeness requires authenticated stock-provider control of a real deterministic page, visible desktop proof, realtime remote proof, failure recovery, and zero helper processes after every close path.

---

## File map

- Create: `src/browser/domain.rs`
- Create: `src/browser/protocol.rs`
- Create: `src/browser/service.rs`
- Create: `src/browser/surface.rs`
- Create: `src/browser/projection.rs`
- Create: `src/browser/teardown.rs`
- Refactor: `src/browser/host/{mod,initialization,windows,unsupported}.rs`
- Refactor: `src/browser/{model,pane,provider,gateway,mcp,commands,operation_queue,automation,policy,storage,resources}.rs`
- Refactor: `src/browser/{attachments,downloads,annotations}.rs`
- Refactor: `src/browser/{recording,recording_coordinator,recording_ipc,recording_mcp}.rs`
- Refactor: `src/browser/{recipes,replay,replay_executor,replay_repair,replay_secrets}.rs`
- Refactor: `src/browser/workflow_mcp.rs`
- Modify: `src/domain/{resource,command,event,snapshot}.rs`
- Modify: `src/kernel/runtime.rs`
- Modify: `src/host/{mod,shutdown}.rs`
- Modify: `src/process/{identity,registry,teardown}.rs`
- Modify: `src/protocol/{capabilities,envelope}.rs`
- Modify: `src/client/action.rs`
- Create: `src/ui/task_cockpit/browser_panel.rs`
- Modify: `src/ui/task_cockpit/context_dock.rs`
- Create: `src/bin/browser-fixture-server.rs`
- Create: `tests/browser_surface.rs`
- Create: `tests/browser_service.rs`
- Create: `tests/browser_gateway.rs`
- Create: `tests/browser_projection.rs`
- Create: `tests/browser_recording.rs`
- Create: `tests/browser_recovery.rs`
- Create: `tests/browser_provider_e2e.rs`
- Create: `tests/fixtures/browser/site/*`
- Create: `scripts/native-next/Invoke-BrowserSurfaceProof.ps1`
- Create: `scripts/native-next/Invoke-BrowserProviderE2E.ps1`
- Create: `docs/adr/0001-host-owned-webview2-surface.md`

### Task 7.1: Prove the host-owned WebView2 surface boundary before refactoring

**Files:** `src/browser/surface.rs`, `src/browser/host/windows.rs`, `src/bin/{devmanager-host,devmanager-next,browser-fixture-server}.rs`, `src/ui/task_cockpit/browser_panel.rs`, `tests/browser_surface.rs`, `scripts/native-next/Invoke-BrowserSurfaceProof.ps1`, `docs/adr/0001-host-owned-webview2-surface.md`

**Proof contract:** the host creates the WebView2 controller beneath a host-owned parking window; it transmits a duplicated/validated child HWND descriptor to the client; GPUI attaches that child to a dedicated native container; hide/detach returns it to the parking window without destroying context; host remains sole WebView2/operation owner.

- [ ] **Step 1: Write failing surface tests** for HWND owner/process identity, attach, resize, hide, detach, reattach, client crash, host crash, stale bounds generation, task switch, 100/125/150/200% DPI, minimize/restore, and zero browser process tree after context close.
- [ ] **Step 2: Build a deterministic fixture server** with pages for focus/input, popup, download, upload, clipboard, long scroll, animation, iframe, auth form with fake secrets, slow response, network failure, and intentional renderer crash navigation.
- [ ] **Step 3: Run** `pwsh scripts/native-next/Invoke-BrowserSurfaceProof.ps1 -Stage Red` and retain screenshots, window hierarchy, PIDs, process creation times, DPI/bounds, and the expected missing implementation result.
- [ ] **Step 4: Implement the smallest surface bridge** using validated process/window identity and generation-fenced attach commands. All SetParent/style/bounds/focus calls run on the owning UI/COM thread required by WebView2; no raw HWND is trusted without host-issued nonce/generation.
- [ ] **Step 5: Consume task-switch pointer input** before changing the attached surface. Hide the outgoing surface first, advance focus/bounds epoch, attach/show the incoming surface, and require a later pointer gesture inside it for page input.
- [ ] **Step 6: Run the proof across the DPI/window matrix** and deliberately terminate the GPUI client. The host/browser context must remain alive and reattach to a new client; context close/full quit must yield zero WebView2 helper members.
- [ ] **Step 7: Record the measured result in ADR 0001.** If any required ownership/focus/DPI/reattach/cleanup invariant fails, stop Phase 7 and revise the surface architecture before touching the rest of `src/browser`; do not hide failure behind screenshots.
- [ ] **Step 8: Commit the passing proof** as `feat(browser): prove host-owned webview2 surface`.

### Task 7.2: Define Task-owned browser identities, facts, and commands

**Files:** `src/browser/{domain,protocol}.rs`, `src/domain/{resource,command,event,snapshot}.rs`, `tests/browser_service.rs`

**Contracts:**

```rust
pub struct BrowserRequest {
    pub request_id: BrowserRequestId,
    pub task_id: TaskId,
    pub context_id: BrowserContextId,
    pub tab_id: Option<BrowserTabId>,
    pub generation: u64,
    pub action: BrowserAction,
}
```

- [ ] **Step 1: Write failing tests** for context/tab typed identity, one Task owner, generation mismatch, popup ownership, closed-task rejection, request idempotency, and deterministic browser snapshot replay.
- [ ] **Step 2: Run** `cargo test --test browser_service domain_ -- --nocapture` and record the red result.
- [ ] **Step 3: Define durable facts** for context created/closed, tab opened/closed/selected, committed navigation, permission decision, artifact link, recipe/recording identity, and health transition. Do not persist mutable COM handles or claim current pixels as facts.
- [ ] **Step 4: Define commands and shared ActionCatalog entries** for create/close context, open/close/select tab, navigate, back/forward/reload/stop, set bounds/visibility/focus, capture, automation, download decision, permission decision, record/replay/cancel, and recover.
- [ ] **Step 5: Add privacy classes** and required permissions to every action/result; reject a cross-Task ID even when tab/context IDs are otherwise valid.
- [ ] **Step 6: Run** domain tests and commit as `feat(browser): define task scoped browser contract`.

### Task 7.3: Move WebView2 environment and operation ownership into the host

**Files:** `src/browser/{service,host/mod,host/initialization,host/windows,storage,operation_queue,teardown}.rs`, `src/host/mod.rs`, `tests/{browser_service,browser_recovery}.rs`

- [ ] **Step 1: Write failing tests** for isolated profile directory, create/open/select/close, serialized navigation, cancellation, two Tasks with independent cookies/storage, client detach, context recovery, and exact directory cleanup policy.
- [ ] **Step 2: Run** `cargo test --test browser_service service_ -- --nocapture` and retain the red output.
- [ ] **Step 3: Implement `BrowserService`** on the host with a dedicated WebView2/COM executor, per-context operation sequencer, runtime generation, and surface registry. UI sends commands; it never owns environment/controller objects.
- [ ] **Step 4: Resolve user-data folders** under `ResolvedAppPaths.browser/{task_id}/{browser_context_id}/{generation}` and fail closed if resolution aliases another profile or production.
- [ ] **Step 5: Route browser helper process membership** into an explicit Host-owned `DevManager Browser Host` Job where platform support permits, and reconcile the WebView2 process tree/exit evidence. If WebView2's platform process model prevents direct Job assignment, document exact observed descendants and enforce cleanup through environment/controller teardown plus postcondition scans; never claim Job ownership without proof.
- [ ] **Step 6: On host recovery**, create a new generation from durable tab/navigation recipes and mark prior runtime interrupted; do not claim JS heap, form state, or in-flight downloads survived.
- [ ] **Step 7: Run** service/recovery tests and commit as `refactor(browser): move browser runtime ownership to host`.

### Task 7.4: Refactor automation and operation queues behind the kernel service

**Files:** `src/browser/{automation,commands,operation_queue,provider,annotations,attachments}.rs`, `tests/browser_service.rs`

- [ ] **Step 1: Port current automation fixtures** and write failing tests for navigate, inspect/accessibility tree, locate, click, type, select, scroll, wait, screenshot, evaluate permitted script, tab handling, timeout, cancellation, stale locator, and concurrent requests.
- [ ] **Step 2: Run** `cargo test --test browser_service automation_ -- --nocapture` and save the red result.
- [ ] **Step 3: Convert every operation** to `BrowserRequest` with deadline/cancellation and return a typed progress/result stream. Serialize mutations per tab; permit only explicitly safe concurrent reads.
- [ ] **Step 4: Fence completion by context/tab generation and navigation document ID.** A result from the prior document/generation is rejected even if DOM shape happens to match.
- [ ] **Step 5: Bound DOM/accessibility/screenshot payloads** and store large results as Task artifacts. Redact password fields and configured secret selectors before journal/Connect projection.
- [ ] **Step 6: Run** automation tests against the real local fixture site, not only mocks; commit as `refactor(browser): sequence visible browser automation in host`.

### Task 7.5: Make browser MCP lifecycle Task-scoped and permissioned

**Files:** `src/browser/{gateway,mcp,workflow_mcp,recording_mcp}.rs`, `src/providers/session.rs`, `src/domain/{command,event}.rs`, `tests/browser_gateway.rs`

- [ ] **Step 1: Write failing tests** for Task-bound endpoint/token, provider generation binding, capability list, read/write permissions, cross-Task denial, revoked session, stale generation, concurrent provider calls, cancellation, and server teardown.
- [ ] **Step 2: Run** `cargo test --test browser_gateway -- --nocapture` and retain the red result.
- [ ] **Step 3: Start one host-owned browser tool gateway** with per-AgentSession scoped credentials/capability grants. Inject its supported stock CLI/MCP configuration at provider launch; never expose a global unauthenticated localhost tool.
- [ ] **Step 4: Resolve every tool call** to the bound Task/context/generation and run it through `BrowserService`. Dangerous navigation/download/upload/clipboard/secret operations require policy/approval according to grant.
- [ ] **Step 5: Emit semantic tool progress/results** to the same provider journal while keeping raw page data local by default. Cancellation from provider/user/Task close terminates the exact operation.
- [ ] **Step 6: Stop and verify the MCP listener/resource** when the AgentSession closes; commit as `feat(browser): scope browser tools to provider tasks`.

### Task 7.6: Attach the browser safely in the GPUI context dock

**Files:** `src/ui/task_cockpit/browser_panel.rs`, `src/browser/{surface,projection}.rs`, `src/client/model.rs`, `tests/{browser_surface,browser_projection}.rs`

- [ ] **Step 1: Write failing tests** for task/tab strip, URL/title/security/loading/error, attach/detach, keyboard traversal, context-dock resize, task switching while form focused, pointer consumption, popup selection, and terminal/browser focus transitions.
- [ ] **Step 2: Run** `cargo test --test browser_surface ui_ -- --nocapture` and record the red result.
- [ ] **Step 3: Keep DevManager chrome native GPUI** and attach only page content. Render tabs, address/status, automation progress, approvals, artifact controls, and diagnostics from host projections.
- [ ] **Step 4: Coordinate surface bounds** with monotonically increasing bounds epochs and physical pixel/DPI values. Hide surface before layout transitions that would expose stale coordinates.
- [ ] **Step 5: Advance both shell focus epoch and browser surface epoch** on Task/dock/tab changes. A navigation gesture cannot become a page click, keypress, drag, file choice, or permission answer.
- [ ] **Step 6: Walk the fixture focus page at all DPI scales** with mouse/keyboard/IME and commit as `feat(ui): embed task browser in gpui dock`.

### Task 7.7: Define the transport-neutral browser projection and web renderer

**Files:** `src/browser/projection.rs`, `src/protocol/envelope.rs`, `web/src/browser/{model,BrowserView,InputOverlay}.tsx`, `web/src/browser/*.test.tsx`, `tests/browser_projection.rs`

- [ ] **Step 1: Write Rust/TypeScript fixture tests** for navigation/tab/progress state, screenshot sequence, tile/full-frame updates, stale frame, resize/input coordinates, first-answer-wins approvals, bandwidth reduction, and resync.
- [ ] **Step 2: Run** `cargo test --test browser_projection -- --nocapture` and `npm --prefix web test -- browser` and save the red results.
- [ ] **Step 3: Produce bounded screenshots** only when a projection viewer is subscribed or a provider operation requests them. Prefer change-aware cadence/quality, with an explicit max FPS/bytes budget; metadata events remain immediate.
- [ ] **Step 4: Map remote pointer/touch/keyboard coordinates** through frame ID, content bounds, scale, and browser focus epoch. Reject input against a stale frame/bounds epoch.
- [ ] **Step 5: Render the web fixture client full-screen at phone sizes** with tab/navigation/progress and a visible interaction mode. Never pretend projected screenshot pixels are a local DOM.
- [ ] **Step 6: Prove no manual refresh** on navigate/tool action/task switch/direct-fixture reconnect; commit as `feat(browser): add realtime browser projection`.

### Task 7.8: Complete downloads, clipboard, file chooser, and secret handling

**Files:** `src/browser/{downloads,attachments,policy,replay_secrets}.rs`, `src/workspace/artifacts.rs`, `tests/browser_service.rs`

- [ ] **Step 1: Write failing tests** for download allow/deny/cancel/name collision/hash/artifact, upload from Task artifact, chooser outside workspace, clipboard read/write permission, secret fill without journal leakage, and remote policy restrictions.
- [ ] **Step 2: Run** `cargo test --test browser_service io_ -- --nocapture` and retain the red result.
- [ ] **Step 3: Route downloads into a Task staging directory** with safe filenames, streaming limits, hash, completion/cancel/failure facts, then promote to artifacts or an explicit chosen destination.
- [ ] **Step 4: Resolve file chooser selections** only from approved workspace/artifact IDs. Remote clients cannot submit arbitrary host paths.
- [ ] **Step 5: Gate clipboard and secret use** by source client/role/action. Secret values are fetched host-side from vault references, injected directly into exact fields, zeroized where possible, and absent from event/log/screenshot metadata.
- [ ] **Step 6: Run** real fixture download/upload/clipboard/secret scenarios and commit as `feat(browser): secure browser io and secrets`.

### Task 7.9: Preserve recordings, recipes, replay, repair, and cancellation

**Files:** `src/browser/{recording,recording_coordinator,recording_ipc,recipes,replay,replay_executor,replay_repair,replay_secrets}.rs`, `tests/browser_recording.rs`

- [ ] **Step 1: Port current replay/recording fixtures** and write failing tests for record start/stop, step sequence, screenshot/artifact links, secret placeholders, replay success, stale locator repair, user approval, cancellation at every wait, crash, and zero operations after close.
- [ ] **Step 2: Run** `cargo test --test browser_recording -- --nocapture` and record the red output.
- [ ] **Step 3: Give recordings/recipes stable Task artifact identities** and sequence every capture/replay action through `BrowserService`; remove window-owned coordinator state.
- [ ] **Step 4: Keep secret placeholders separate** from values. Replay resolves allowed secret references at execution and never serializes values into recipes, screenshots, journal, or Connect metadata.
- [ ] **Step 5: Make locator repair an explicit proposed patch** with evidence and approval; persist accepted recipe revisions, never silently mutate the only recipe.
- [ ] **Step 6: Propagate cancellation** from provider/user/task/host into navigation, waits, capture, repair, and replay; verify no queued action resumes after a new generation.
- [ ] **Step 7: Run** tests and commit as `refactor(browser): retain durable recording and replay workflows`.

### Task 7.10: Recover from navigation, renderer, sleep, and client failures

**Files:** `src/browser/{service,teardown,host/windows}.rs`, `src/process/teardown.rs`, `tests/browser_recovery.rs`

- [ ] **Step 1: Write failing tests** for DNS/navigation failure, unresponsive renderer, WebView process crash, client crash while attached, host shutdown, Windows sleep/wake simulation hooks, display/DPI change, failed create, and repeated teardown.
- [ ] **Step 2: Run** `cargo test --test browser_recovery -- --nocapture` and save the red result.
- [ ] **Step 3: Detect process/controller failure** and mark exact context generation unhealthy. Recover into a new generation using durable tab/navigation recipes and surface an interruption marker.
- [ ] **Step 4: On client detach/crash**, park/hide surfaces in the host before accepting a new attachment. On sleep/display change, invalidate bounds/focus epochs and require fresh layout/input evidence.
- [ ] **Step 5: Teardown in order:** cancel operations, deny new input, detach/park surface, close controllers/tabs/environment, await helper disappearance, reconcile ports/files, then mark closed. Any remaining helper becomes a visible leak fault.
- [ ] **Step 6: Run** recovery tests repeatedly and commit as `feat(browser): recover and teardown browser generations`.

### Task 7.11: Prove real LLM-controlled browser operation end to end

**Files:** `tests/browser_provider_e2e.rs`, `scripts/native-next/Invoke-BrowserProviderE2E.ps1`, `docs/browser-e2e-matrix.md`, `docs/replacement-deletion-ledger.md`

- [ ] **Step 1: Define deterministic provider prompts** whose success can be machine-verified on the fixture site: navigate, inspect a value, fill non-secret form, choose an option, submit, open a tab, download an artifact, upload it, handle a permission, and report the final verification token.
- [ ] **Step 2: Add fixture-only protocol tests** and run them red before authenticated execution to prove that mocked adapters cannot satisfy the final acceptance marker.
- [ ] **Step 3: For each installed/authenticated Claude, Codex, and Cursor capability**, launch the stock provider with its Task-scoped browser tools and execute the real prompt. Record provider/version/session ID, browser/Task/generation, journal tool events, page audit events, screenshots, result token, and process tree.
- [ ] **Step 4: While the provider works**, switch desktop Tasks, detach/reattach the GPUI client, connect the direct projection fixture at phone width, resize/change DPI, minimize/restore, and cancel one run. Require realtime progress without refresh and no focus/input leakage. Phase 8 repeats this through real direct/hosted Connect.
- [ ] **Step 5: Execute recovery cases** for navigation error, renderer crash, provider crash, host full quit, and failed browser launch. Every case must settle in the kernel with truthful status.
- [ ] **Step 6: Close each Task and require zero** provider, MCP, browser, WebView2, fixture-server, downloader, recorder, and test helper members/listeners. Hash the isolated browser profile and verify production browser/profile paths were never opened.
- [ ] **Step 7: Document the full matrix** with pass/fail evidence; any unsupported provider capability is labeled as such and cannot be counted as a passing controlled-browser case.
- [ ] **Step 8: Update the deletion ledger** for all old browser ownership/bridge paths; commit as `test(browser): validate real provider controlled browsing`.

## Phase 7 verification gate

- [ ] Capture production hashes/PID/start time and announce the long WebView2/provider test gate.
- [ ] Run `cargo test --test browser_surface --test browser_service --test browser_gateway --test browser_projection --test browser_recording --test browser_recovery --test browser_provider_e2e -- --nocapture`.
- [ ] Run the web browser tests and `npm --prefix web run typecheck && npm --prefix web run build`.
- [ ] Run `pwsh scripts/native-next/Invoke-BrowserSurfaceProof.ps1 -AllDpi -ClientCrash -HostRecovery`.
- [ ] Run `pwsh scripts/native-next/Invoke-BrowserProviderE2E.ps1 -Providers Available -IncludeProjectionFixture -IncludeRecovery` with explicit authenticated-smoke confirmation.
- [ ] Run `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`.
- [ ] Review every surface/E2E screenshot, process tree, event trace, cancellation trace, and cleanup assertion.
- [ ] Confirm no Cargo/rustc/test/provider/MCP/fixture/WebView2/development host remains and all owned ports are free.
- [ ] Compare production config/remote hashes, installed PID/start time, and production browser-data metadata.
- [ ] Review the complete Phase 7 diff and deletion ledger.

## Phase 7 exit criteria

- A host-owned WebView2 context visibly attaches/parks/reattaches across GPUI client lifecycle and all supported DPI/focus transitions.
- Browser identity, operations, automation, MCP, downloads, secrets, recordings, and replay are Task/generation scoped and host owned.
- Desktop and the transport-neutral/direct fixture observe/control the same browser in realtime without creating a second browser; Phase 8 must prove the same contract over direct and hosted Connect.
- At least Claude and Codex complete the deterministic real browser scenario when their installed stock CLIs expose the required browser-tool capability; Cursor is tested to its truthful probed capability.
- Crash/cancel/sleep/navigation/focus cases reconcile honestly and every close path leaves zero owned browser/provider/tool helpers.
- Production DevManager and browser data remain untouched.
