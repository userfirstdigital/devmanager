# Native Transcript and Process Monitor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a provider-first native web transcript, continuous native log views, quiet automatic reconnection, and a compact project-aware desktop process monitor that cannot click through.

**Architecture:** Normalized Rust semantic events remain the source of web content. A pure TypeScript presentation model converts those events into conversational messages, grouped activity, actionable notices, and log runs before React renders them. The GPUI process monitor receives a pure project-aware view model derived from existing `AppState` and `RuntimeState`; it does not own process state.

**Tech Stack:** Rust 2021, GPUI 0.2.2, React 18, TypeScript 5.6, Zustand 5, Vitest 4, Testing Library, `react-markdown`, `remark-gfm`, Vite 6.

## Global Constraints

- Native DevManager remains the only owner of process lifecycle and runtime state.
- Structured Claude/Codex events are authoritative; degraded terminal projection is best-effort and bounded.
- The browser never resurrects runtime data after `runtimeInstanceId` changes.
- Short reconnects are silent; sustained offline state appears after exactly seven seconds with no Resume or Reconnect button.
- Raw Terminal remains available as the advanced fallback.
- Raw Markdown HTML is disabled.
- The desktop monitor derives project/type labels without creating another state store.
- Every production behavior starts with a failing test and ends with targeted plus final verification.

---

### Task 1: Conversation and Activity Presentation Model

**Files:**
- Create: `web/src/sessions/timeline/timelineModel.ts`
- Create: `web/src/sessions/timeline/timelineModel.test.ts`
- Create: `web/src/sessions/timeline/MarkdownMessage.tsx`
- Create: `web/src/sessions/timeline/MarkdownMessage.test.tsx`
- Modify: `web/src/sessions/timeline/Timeline.tsx`
- Modify: `web/src/sessions/timeline/eventRenderers.tsx`
- Modify: `web/src/sessions/timeline/eventRenderers.test.tsx`
- Modify: `web/src/index.css`
- Modify: `web/package.json`
- Modify: `web/package-lock.json`

**Interfaces:**
- Produces: `buildConversationItems(events, density): ConversationItem[]` where items are `message`, `activity`, `question`, `error`, or `fallbackOutput`.
- Produces: `MarkdownMessage({ text }: { text: string }): JSX.Element` using `react-markdown` and `remark-gfm` without `rehype-raw`.
- Consumes: existing `SemanticEvent` and `InterfaceDensity`.

- [ ] **Step 1: Write failing model tests**

Add fixtures proving assistant events with the same `message_id` replace in place, routine statuses and `terminalMode` events disappear, consecutive tool/reasoning/diff/command events become one activity item, and failures/questions stay top-level.

```ts
expect(buildConversationItems(events, "calm")).toMatchObject([
  { kind: "message", role: "user", text: "Fix it" },
  { kind: "activity", count: 2, state: "success" },
  { kind: "message", role: "assistant", text: "Done", streaming: false },
]);
expect(items.some((item) => item.kind === "status")).toBe(false);
```

- [ ] **Step 2: Verify RED**

Run: `npm test -- src/sessions/timeline/timelineModel.test.ts`
Expected: FAIL because `timelineModel.ts` does not exist.

- [ ] **Step 3: Implement the pure model**

Define discriminated item types. Key assistant replacement by `message_id`, flush activity before messages/actionable events, summarize action names and counts, and retain only one bounded degraded-output block between conversation events.

```ts
export type ConversationItem =
  | { kind: "message"; key: string; sequence: number; role: "user" | "assistant"; text: string; streaming: boolean }
  | { kind: "activity"; key: string; sequence: number; events: SemanticEvent[]; count: number; state: "active" | "success" | "failure" }
  | { kind: "question"; key: string; sequence: number; event: QuestionEvent }
  | { kind: "error"; key: string; sequence: number; event: ErrorEvent }
  | { kind: "fallbackOutput"; key: string; sequence: number; text: string; stream: SemanticStream };
```

- [ ] **Step 4: Verify model GREEN**

Run: `npm test -- src/sessions/timeline/timelineModel.test.ts`
Expected: all model tests pass.

- [ ] **Step 5: Write failing Markdown and renderer tests**

Assert headings, lists, GFM table/task-list output, fenced code, safe external links, raw HTML suppression, one `N actions` disclosure, user bubble, and full-width assistant prose. Assert the DOM contains no `Output`, `Starting`, `Running`, or `Native view restored` card for an AI fixture.

- [ ] **Step 6: Verify renderer RED**

Run: `npm test -- src/sessions/timeline/MarkdownMessage.test.tsx src/sessions/timeline/eventRenderers.test.tsx`
Expected: FAIL because Markdown and grouped-activity rendering are absent.

- [ ] **Step 7: Add safe Markdown and grouped renderers**

Install `react-markdown` and `remark-gfm`. Render raw HTML as text/ignored content by leaving `rehype-raw` unconfigured. External links receive `target="_blank"` and `rel="noreferrer noopener"`. Render activity as one `<details>` disclosure and assistant prose without a card shell.

- [ ] **Step 8: Replace Timeline's event-by-event rendering**

Build `ConversationItem[]` through `useMemo`, retain scroll anchoring by item sequence, and render item components. Keep `role="log"`, native selection, follow-bottom behavior, and streaming `aria-busy`.

- [ ] **Step 9: Verify Task 1**

Run: `npm test -- src/sessions/timeline`
Expected: all timeline tests pass with no warnings.

- [ ] **Step 10: Commit**

Run `git add web/package.json web/package-lock.json web/src/sessions/timeline web/src/index.css && git commit -m "feat(web): render native AI conversations"`.

---

### Task 2: Continuous Server, Shell, and SSH Logs

**Files:**
- Create: `web/src/sessions/timeline/LogTimeline.tsx`
- Create: `web/src/sessions/timeline/LogTimeline.test.tsx`
- Modify: `web/src/sessions/views/ServerSessionView.tsx`
- Create: `web/src/sessions/views/ServerSessionView.test.tsx`
- Modify: `web/src/sessions/views/CommandSessionView.tsx`
- Modify: `web/src/sessions/views/CommandSessionView.test.tsx`
- Modify: `web/src/index.css`

**Interfaces:**
- Produces: `buildLogRows(events): LogRow[]` and `LogTimeline({ events, emptyTitle, emptyDetail })`.
- A `LogRow` is `output`, `command`, `runBoundary`, `error`, or `question`; adjacent same-stream output is coalesced.

- [ ] **Step 1: Write failing log tests**

Assert adjacent stdout chunks become one `<pre>`, stderr has a semantic error class without an accordion, restart/start status creates a run boundary, commands show exit state, and the user can pause bottom-following then use a `New output` button.

- [ ] **Step 2: Verify RED**

Run: `npm test -- src/sessions/timeline/LogTimeline.test.tsx`
Expected: FAIL because `LogTimeline` does not exist.

- [ ] **Step 3: Implement continuous log projection**

Ignore routine state cards, convert `starting` after prior output into a new run boundary, preserve crash/exit/error rows, and render wrapping monospaced output directly in a native log surface.

- [ ] **Step 4: Verify log GREEN**

Run: `npm test -- src/sessions/timeline/LogTimeline.test.tsx`
Expected: all log tests pass.

- [ ] **Step 5: Write failing view integration tests**

Assert server status/port/process/controls stay compact and output text is immediately visible. Assert shell/SSH output and commands are visible without an `Output` disclosure.

- [ ] **Step 6: Verify view RED**

Run: `npm test -- src/sessions/views/ServerSessionView.test.tsx src/sessions/views/CommandSessionView.test.tsx`
Expected: FAIL because both views still use the generic card timeline.

- [ ] **Step 7: Switch server and command views to LogTimeline**

Keep current mutation callbacks and composer behavior. Replace only presentation and CSS density.

- [ ] **Step 8: Verify Task 2**

Run: `npm test -- src/sessions/timeline/LogTimeline.test.tsx src/sessions/views/ServerSessionView.test.tsx src/sessions/views/CommandSessionView.test.tsx`
Expected: all tests pass.

- [ ] **Step 9: Commit**

Run `git add web/src/sessions web/src/index.css && git commit -m "feat(web): stream continuous native logs"`.

---

### Task 3: Quiet Seven-Second Reconnection State

**Files:**
- Create: `web/src/app/useOfflineIndicator.ts`
- Create: `web/src/app/useOfflineIndicator.test.tsx`
- Modify: `web/src/app/AppShell.tsx`
- Modify: `web/src/app/AppShell.test.tsx`
- Modify: `web/src/index.css`

**Interfaces:**
- Produces: `useOfflineIndicator(status: WsStatus, delayMs = 7000): boolean`.
- Consumes the current socket status only; authoritative reconciliation is already represented by the transition to `open`.

- [ ] **Step 1: Write fake-timer RED tests**

Assert `connecting` is silent, `closed` is silent at 6,999 ms, `closed` shows at 7,000 ms, and changing to `open` clears immediately. Preserve cached children throughout and assert there is no Resume/Reconnect button.

- [ ] **Step 2: Verify RED**

Run: `npm test -- src/app/useOfflineIndicator.test.tsx src/app/AppShell.test.tsx`
Expected: FAIL because the current banner appears immediately.

- [ ] **Step 3: Implement timer hook and compact indicator**

Use an effect with a cleared timeout on every status change/unmount. Render `Offline · reconnecting` as a compact status chip only when the hook returns true. Keep `unauthorized` handled by the pairing screen.

- [ ] **Step 4: Verify Task 3**

Run: `npm test -- src/app/useOfflineIndicator.test.tsx src/app/AppShell.test.tsx`
Expected: all reconnect tests pass.

- [ ] **Step 5: Commit**

Run `git add web/src/app web/src/index.css && git commit -m "fix(web): keep brief reconnects silent"`.

---

### Task 4: Compact Project-Aware Desktop Process Monitor

**Files:**
- Modify: `src/app/process_monitor.rs`
- Modify: `src/app/mod.rs`

**Interfaces:**
- Produces: `ProcessMonitorEntry::from_session(app_state: &AppState, session: &SessionRuntimeState) -> ProcessMonitorEntry` through `process_monitor_entries(app_state, runtime)`.
- Extends: `render_process_monitor(state, app_state, runtime, actions)`.
- Keeps: current `ProcessMonitorAction` variants and process-manager stop/kill wiring.

- [ ] **Step 1: Write failing view-model tests**

Construct two same-named sessions in different projects plus Server, SSH, Claude, Codex, and Shell fixtures. Assert `project_name`, explicit kind label, status label, deterministic ordering, totals, missing-project fallback, and child ordering.

```rust
let entries = process_monitor_entries(&app_state, &runtime);
assert_eq!(entries[0].project_name, "360 Portal");
assert_eq!(entries[0].kind_label, "Codex");
assert_eq!(entries[1].project_name, "DevManager");
```

- [ ] **Step 2: Verify RED**

Run: `cargo test --lib app::process_monitor::tests::process_monitor_entries`
Expected: FAIL because the view-model function and fields do not exist.

- [ ] **Step 3: Implement the pure view model**

Resolve project by `session.project_id`; fall back through tab/command metadata only when needed. Use `SessionKind` for kind labels. Sort problem/live states first, then project, kind, label, stable ID.

- [ ] **Step 4: Verify model GREEN**

Run: `cargo test --lib app::process_monitor::tests`
Expected: all process-monitor model tests pass.

- [ ] **Step 5: Add a failing structural propagation test**

Extract a small `consume_modal_pointer(event, window, cx)` handler and a `consume_action_pointer` wrapper. Test that the renderer source path uses the consuming handler for the panel and nested action controls while the backdrop keeps `Close`.

- [ ] **Step 6: Verify propagation RED**

Run: `cargo test --lib app::process_monitor::tests::modal_interactions_consume_internal_pointer_events`
Expected: FAIL because the frame currently installs an empty handler.

- [ ] **Step 7: Stop propagation and compact the renderer**

Call `cx.stop_propagation()` inside the frame pointer handler and before Toggle/Stop/Kill actions. Render a compact two-line row containing label, type badge, status, project, PID/process count, CPU, and memory. Keep Stop separate and expansion inline.

- [ ] **Step 8: Pass AppState at the render boundary**

Update `src/app/mod.rs` to pass the current app state snapshot/reference with the runtime snapshot. Do not add monitor-owned project data.

- [ ] **Step 9: Verify Task 4**

Run: `cargo test --lib app::process_monitor::tests`
Expected: all process-monitor tests pass.

Run: `cargo fmt --check`
Expected: no formatting diff.

- [ ] **Step 10: Commit**

Run `git add src/app/process_monitor.rs src/app/mod.rs && git commit -m "fix(desktop): make process monitor compact and stable"`.

---

### Task 5: Degraded AI Projection and Adapter Diagnostics

**Files:**
- Modify: `src/remote/presentation.rs`
- Modify: `src/services/process_manager.rs`
- Modify: `src/ai/codex_bridge.rs` only if live capability evidence identifies a bridge defect.
- Modify: `web/src/sessions/views/AiSessionView.tsx`
- Create or modify corresponding tests in the same Rust modules and `web/src/sessions/views/AiSessionView.test.tsx`.

**Interfaces:**
- Produces bounded, replacement-oriented degraded AI output rather than unbounded byte-chunk concatenation.
- Preserves line-oriented append projection for Server, Shell, and SSH.
- Keeps `SemanticAdapterHealth` wire-compatible; diagnostic reasons remain host-side logs/diagnostics unless a safe additive field is already available.

- [ ] **Step 1: Capture current Codex bridge evidence**

Run the installed CLI help probes for `app-server --listen`, `--remote`, and remote-auth. Launch through the hot-load app and record the adapter health transition and exact startup/negotiation error. Form one root-cause hypothesis before editing bridge code.

- [ ] **Step 2: Write the failing Rust regression**

For the confirmed bridge defect, add the smallest fixture reproducing the actual startup/handshake failure. Separately add a projector test feeding cursor-up/clear-screen redraws and assert native output replaces the current fallback snapshot instead of duplicating frames.

- [ ] **Step 3: Verify RED**

Run: `cargo test --lib remote::presentation::tests::degraded_ai_screen_redraw_replaces_in_progress_output`

When the live evidence identifies a bridge defect, also run: `cargo test --lib ai::codex_bridge::tests::current_cli_remote_capabilities_activate_bridge`
Expected: each test fails for the observed missing behavior.

- [ ] **Step 4: Implement the minimal root fixes**

Fix only the confirmed bridge boundary. For degraded AI output, apply terminal-aware/replacement projection at the existing presentation boundary and cap fallback text. Do not alter Server/Shell append semantics.

- [ ] **Step 5: Verify GREEN**

Run: `cargo test --lib ai::codex_bridge::tests`

Run: `cargo test --lib remote::presentation::tests`

Expected: all adapter and presentation tests pass.

- [ ] **Step 6: Verify the single fallback notice**

Add/render an AI fixture with degraded health and repeated status/output events. Assert one compact `Limited transcript detail` notice and no routine lifecycle cards.

- [ ] **Step 7: Commit**

Run `git add src/ai src/remote/presentation.rs src/services/process_manager.rs web/src/sessions/views && git commit -m "fix(ai): stabilize native transcript projection"`.

---

### Task 6: Build, Hot-Load, and Acceptance Verification

**Files:**
- Regenerate: `web/bundle/**`
- Update plan checkboxes and implementation notes in this document.

**Interfaces:**
- Uses `dev-watch.ps1` with its isolated `DEVMANAGER_PROFILE=dev-watch` namespace.
- Uses a non-production browser listener port configured in that profile.

- [ ] **Step 1: Run complete automated web verification**

Run: `npm test && npm run build`
Expected: all Vitest files pass, TypeScript passes, and Vite regenerates the tracked bundle.

- [ ] **Step 2: Run complete Rust verification serially**

Run: `cargo fmt --check`

Run: `cargo test --lib -- --test-threads=1`

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: zero failures and zero warnings. The serial test setting avoids the confirmed baseline profile-lock race.

- [ ] **Step 3: Start hot-load DevManager**

Run `powershell -ExecutionPolicy Bypass -File .\dev-watch.ps1` from the worktree. Wait for the watcher to report a launched app under the `dev-watch` profile. Configure/confirm its browser listener on a separate port and pair the test browser if the isolated profile requires it.

- [ ] **Step 4: Browser acceptance at 390 x 844**

Using the in-app browser, send a real Codex prompt, verify one user bubble and one streaming Markdown response, expand grouped actions, start/stop/restart a server and read its continuous logs, open Raw Terminal explicitly, reload, background/return, and verify short reconnects show no warning. Capture screenshots of AI and server views.

- [ ] **Step 5: Native desktop process-monitor acceptance**

Open the bottom `terminals open` monitor. Verify every row shows project and type, row clicks expand/collapse without closing, Stop does not toggle/close, backdrop closes, and no sidebar item activates through the sheet. Capture a screenshot.

- [ ] **Step 6: Final diff and requirement audit**

Run `git diff master...HEAD --check`, inspect every changed file, and compare the result line-by-line with the approved design acceptance criteria. Record any unavailable external Claude success path separately; do not substitute it with an unverified claim.

- [ ] **Step 7: Commit generated bundle and verification notes**

Run `git add web/bundle docs/superpowers/plans/2026-07-14-native-transcript-and-process-monitor.md && git commit -m "build: embed native transcript web bundle"`.

- [ ] **Step 8: Finish the development branch**

Use `superpowers:finishing-a-development-branch`, merge the verified branch to master as requested in the established workflow, remove the worktree after a clean merge, push master, and verify the release workflow if the resulting commit is intended to release.
