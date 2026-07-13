# Native Mobile Web Interface Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the terminal-first remote browser with a host-authoritative, installable iPhone-first PWA that renders Claude, Codex, server, shell, and SSH sessions as native semantic views with seamless resume and automatic terminal fallback.

**Architecture:** The existing PTY and native DevManager process remain execution truth. A redacted web-only protocol exposes stable session summaries plus bounded in-memory semantic journals; React renders those journals as native DOM and lazy-loads xterm only for terminal-grid interactions. Runtime epochs, connection-scoped writer leases, acknowledged input, installed-only route restore, and service-worker/push integration make suspend/reconnect behavior deterministic.

**Tech Stack:** Rust 2021, Tokio/Axum WebSocket, Serde, portable-pty, React 18, Zustand 5, TypeScript 5.6, Vite 6, Vitest 4, Tailwind CSS 4, xterm 5, vite-plugin-pwa/Workbox, Web Push.

## Global Constraints

- The native DevManager process is the only source of runtime truth; no journal or transcript survives its restart.
- Normal mobile views use native DOM and a real HTML textarea; xterm is a lazy automatic/manual fallback.
- Stable browser identity uses command IDs for servers and tab IDs for Claude, Codex, and SSH; never route by PTY session ID.
- A changed runtime instance clears browser journals, drafts, unread state, and last-session restoration.
- Browser snapshots/actions are allowlisted and never contain SSH secrets, tokens, environment values, startup commands, or unrelated provider data.
- Input never waits for a Take Control, Resume, or Reconnect button; the host still enforces one connection-scoped writer.
- Plain LAN HTTP remains an operating fallback, while installability, service workers, attachments requiring secure APIs, and push require HTTPS.
- Provider adapters fail open to the unchanged PTY command and terminal projection.
- Batch Rust verification after coherent backend groups; do not run Cargo after every small edit.

---

## File structure

### Rust

- `src/remote/web/dto.rs`: allowlisted browser snapshots, session summaries, conversion/redaction.
- `src/remote/web/action.rs`: allowlisted browser actions and conversion to internal actions.
- `src/remote/web/wire.rs`: versioned web frames, resume, lease, semantic subscription, acknowledged composer input.
- `src/remote/presentation.rs`: stable keys, semantic event types, bounded journals, ANSI/plain-text projector.
- `src/remote/web/lease.rs`: connection-scoped writer lease reducer with injectable clock.
- `src/remote/web/bridge.rs`: web connection orchestration and translation; no desktop snapshot passthrough.
- `src/ai/claude_hooks.rs`: session-scoped settings, relay registry, hook reducer.
- `src/ai/codex_bridge.rs`: transparent app-server proxy and Codex event observer.
- `src/ai/mod.rs`: adapter types shared with process/session lifecycle.
- `src/remote/web/push.rs`: push subscriptions, VAPID sender, attention payloads.
- `src/main.rs`: early Claude hook relay mode.
- `src/services/process_manager.rs`: adapter-aware AI launch/fallback and adapter teardown.
- `src/remote/mod.rs`: runtime instance, revision, presentation store, and web-facing accessors.
- `src/remote/web/mod.rs`: authenticated HTTP routes for push and internal hook relay.
- `src/remote/web/assets.rs`: PWA/static cache and security headers.
- `build.rs`: deterministic web bundle validation.

### Web

- `web/src/app/AppShell.tsx`: responsive native shell and destinations.
- `web/src/app/router.ts`: History API route parser/navigator.
- `web/src/app/restore.ts`: installed-only runtime-scoped route restore.
- `web/src/platform/lifecycle.ts`: visibility, stale-socket, viewport, and standalone helpers.
- `web/src/sessions/sessionKey.ts`: stable session-key parsing and mapping.
- `web/src/sessions/sessionModel.ts`: active/attention/recent grouping and presentation selectors.
- `web/src/sessions/SessionsScreen.tsx`: active/recent native home.
- `web/src/sessions/SessionScreen.tsx`: shared navigation and per-kind routing.
- `web/src/sessions/Composer.tsx`: native textarea, attachment input, acknowledged submit.
- `web/src/sessions/timeline/Timeline.tsx`: scroll anchoring and semantic events.
- `web/src/sessions/timeline/eventRenderers.tsx`: calm/minimal/full cards.
- `web/src/sessions/views/*.tsx`: AI, server, command, and raw terminal views.
- `web/src/projects/*.tsx`: project index/detail launch flows.
- `web/src/settings/SettingsScreen.tsx`: density, notification, install/security diagnostics.
- `web/src/drafts/draftStore.ts`: runtime-scoped, bounded local draft persistence.
- `web/src/pwa/register.ts`: controlled service-worker lifecycle.
- `web/src/pwa/notifications.ts`: permission/subscription/badge helpers.
- `web/src/sw.ts`: app-shell caching, push, and notification click behavior.
- `web/public/icons/*`: 180/192/512 and maskable app icons.
- `examples/generate_pwa_icons.rs`: deterministic resizing/padding of the established app icon.
- `web/src/api/types.ts`, `web/src/api/ws.ts`, `web/src/store/index.ts`: new protocol and state reconciliation.

---

### Task 1: Establish the redacted web protocol boundary

**Files:**
- Create: `src/remote/web/dto.rs`
- Create: `src/remote/web/action.rs`
- Modify: `src/remote/web/mod.rs`
- Modify: `src/remote/web/wire.rs`
- Modify: `src/remote/web/bridge.rs`
- Modify: `src/remote/mod.rs`
- Test: inline `#[cfg(test)]` modules in the new files and `wire.rs`

**Interfaces:**
- Consumes: `AppState`, `RuntimeState`, `RemoteAction`, and `PortStatus` from existing native modules.
- Produces: `WebWorkspaceSnapshot::from_host`, `WebWorkspaceDelta`, `WebSessionSummary`, `WebAction::into_remote`, and `WEB_PROTOCOL_VERSION`.

- [ ] **Step 1: Write failing redaction and action allowlist tests**

```rust
#[test]
fn browser_snapshot_never_serializes_host_secrets() {
    let fixture = host_fixture_with_sentinels();
    let value = serde_json::to_string(&WebWorkspaceSnapshot::from_host(
        "runtime-1", 7, &fixture.app, &fixture.runtime, &fixture.ports, &fixture.lease,
    )).unwrap();
    for forbidden in ["PASSWORD_SENTINEL", "PRIVATE_KEY_SENTINEL", "TOKEN_SENTINEL",
                      "ENV_SENTINEL", "STARTUP_SENTINEL", "NOTES_SENTINEL"] {
        assert!(!value.contains(forbidden), "leaked {forbidden}");
    }
}

#[test]
fn web_action_parser_rejects_native_configuration_replacement() {
    let raw = serde_json::json!({"type":"saveSsh","connection":{"password":"secret"}});
    assert!(serde_json::from_value::<WebAction>(raw).is_err());
}
```

- [ ] **Step 2: Run the focused Rust tests and verify they fail**

Run: `cargo test remote::web --lib`  
Expected: compilation fails because `WebWorkspaceSnapshot` and `WebAction` do not exist.

- [ ] **Step 3: Implement allowlisted DTOs and actions**

Use these public shapes and only add fields required by a renderer:

```rust
pub const WEB_PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebWorkspaceSnapshot {
    pub web_protocol_version: u32,
    pub runtime_instance_id: String,
    pub revision: u64,
    pub server_id: String,
    pub projects: Vec<WebProject>,
    pub ssh_connections: Vec<WebSshConnection>,
    pub tabs: Vec<WebTab>,
    pub sessions: Vec<WebSessionSummary>,
    pub port_statuses: Vec<WebPortStatus>,
    pub writer_lease: WebWriterLeaseState,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebWriterLeaseState {
    pub owner_client_instance_id: Option<String>,
    pub generation: u64,
    pub expires_at_epoch_ms: Option<u64>,
    pub you_are_owner: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum WebAction {
    StartServer { command_id: String },
    StopServer { command_id: String },
    RestartServer { command_id: String },
    LaunchAi { project_id: String, tab_type: WebAiKind },
    RestartAiTab { tab_id: String },
    CloseTab { tab_id: String },
    OpenSshTab { connection_id: String },
    ConnectSsh { connection_id: String },
    RestartSsh { connection_id: String },
    DisconnectSsh { connection_id: String },
    StopAllServers,
}
```

Sanitized project commands expose IDs, labels, ports, and live state but not command/args/env. Sanitized SSH connections expose ID, label, host, port, and username but not password/private key. Settings are omitted until an allowlisted presentation setting is needed.

Create one `runtime_instance_id` with the existing `generate_secret("runtime")` helper inside `RemoteHostService::new`; store it only on `RemoteHostInner` and expose it to the web snapshot conversion. Add the current workspace revision to the safe projection. Neither value is persisted.

- [ ] **Step 4: Translate bridge snapshots/deltas and action requests**

Replace direct `RemoteWorkspaceSnapshot`/`RemoteWorkspaceDelta` serialization in `wire.rs` and `bridge.rs`. Convert `WebAction` to `RemoteAction` server-side with host-supplied default dimensions. Emit a safe full web snapshot on native state changes initially; optimize to safe deltas only after profiling.

- [ ] **Step 5: Run focused tests and inspect serialized JSON**

Run: `cargo test remote::web --lib`  
Expected: all web protocol, redaction, auth, image-paste, and bridge tests pass.

- [ ] **Step 6: Commit**

```powershell
git add src/remote/web
git commit -m "security: isolate the browser protocol"
```

### Task 2: Add runtime identity, stable session keys, and semantic journals

**Files:**
- Create: `src/remote/presentation.rs`
- Modify: `src/remote/mod.rs`
- Modify: `src/remote/web/dto.rs`
- Modify: `src/remote/web/bridge.rs`
- Test: inline tests in `presentation.rs` and `remote/mod.rs`

**Interfaces:**
- Consumes: `SessionRuntimeState`, `SessionTab`, PTY output chunks, and screen mode metadata.
- Produces: `StableSessionKey`, `SemanticEvent`, `SemanticJournalStore`, `runtime_instance_id`, and `snapshot_revision`.

- [ ] **Step 1: Write failing identity, bounded replay, and projector tests**

```rust
#[test]
fn stable_keys_never_use_pty_ids() {
    assert_eq!(StableSessionKey::from_server("cmd-1").to_string(), "server:cmd-1");
    assert_eq!(StableSessionKey::from_tab("tab-1").to_string(), "tab:tab-1");
}

#[test]
fn journal_replays_strictly_after_cursor_and_reports_rollover() {
    let mut journal = SemanticJournal::with_limits(JournalLimits {
        canonical_events: 3, canonical_bytes: 1024,
        verbose_events: 2, verbose_bytes: 128,
    });
    for text in ["one", "two", "three", "four"] { journal.push(canonical_output(text)); }
    let replay = journal.replay_after(1);
    assert_eq!(replay.oldest_sequence, 2);
    assert!(replay.cursor_rolled_over);
    assert_eq!(replay.events.iter().map(|e| e.sequence).collect::<Vec<_>>(), vec![2,3,4]);
}

#[test]
fn ansi_projector_handles_sequences_split_across_chunks() {
    let mut projector = PlainTextProjector::default();
    assert_eq!(projector.push(b"ok\x1b[3"), "ok");
    assert_eq!(projector.push(b"1mred\x1b[0m\rnext\n"), "red\nnext\n");
}
```

- [ ] **Step 2: Run the focused tests and verify failure**

Run: `cargo test remote::presentation::tests --lib`  
Expected: compilation fails because the presentation module is absent.

- [ ] **Step 3: Implement stable keys, normalized events, and bounded journals**

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StableSessionKey(String);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticEvent {
    pub stable_session_key: StableSessionKey,
    pub sequence: u64,
    pub occurred_at_epoch_ms: u64,
    pub source: SemanticSource,
    #[serde(flatten)]
    pub kind: SemanticEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SemanticEventKind {
    UserMessage { text: String },
    AssistantMessage { message_id: String, text: String, streaming: bool },
    Reasoning { item_id: String, summary: String },
    Tool { tool_id: String, name: String, state: SemanticToolState, summary: String },
    Diff { item_id: String, unified_diff: String },
    Command { command_id: String, text: String, exit_code: Option<i32> },
    Output { stream: SemanticStream, text: String },
    Question { question_id: String, prompt: String, choices: Vec<String> },
    Status { state: String, detail: Option<String> },
    Error { message: String },
    TerminalMode { raw_required: bool },
}
```

The store owns per-key sequence counters, tiered byte/count eviction, deduplication keys, activity timestamps, attention state, and adapter health. Nothing is persisted. Canonical user/assistant/command/question/final-summary events target 50,000 entries or 64 MiB per session; verbose log/delta/tool output has a separate rolling budget and collapses to a truncation marker first.

- [ ] **Step 4: Add stable-key mapping and semantic revision updates**

Use the runtime ID and initial revision from Task 1. Implement stable-key resolution from runtime/tab data and increment the revision whenever a web-visible semantic projection changes. Keep both values out of persisted config.

- [ ] **Step 5: Feed PTY/runtime events into the store**

At the existing central `RemoteSessionStreamEvent::Output` publication seam, map session ID to a stable key and feed bytes into a per-session `PlainTextProjector`. Emit bounded Output events for server/shell/SSH and a degraded AI fallback. Emit Status and terminal mode events from runtime/bootstrap changes. Provider adapters may later replace the degraded AI projection without changing the journal API.

- [ ] **Step 6: Run backend tests**

Run: `cargo test remote:: --lib`  
Expected: all remote and presentation tests pass.

- [ ] **Step 7: Commit**

```powershell
git add src/remote
git commit -m "feat: add host semantic session journals"
```

### Task 3: Implement atomic resume, session generations, and automatic writer leases

**Files:**
- Create: `src/remote/web/lease.rs`
- Modify: `src/remote/web/wire.rs`
- Modify: `src/remote/web/bridge.rs`
- Modify: `src/remote/mod.rs`
- Test: inline lease/wire/bridge tests

**Interfaces:**
- Consumes: connection ID, browser install client ID, client instance ID, visibility, stable session key, runtime/revision cursors.
- Produces: `ResumeRequest`, `ResumeState`, `WriterLease`, acknowledged `ComposerAccepted`, and stale-generation errors.

- [ ] **Step 1: Write failing deterministic lease tests using a fake timestamp**

```rust
#[test]
fn second_tab_cannot_write_with_the_same_install_cookie() {
    let mut leases = WriterLeaseManager::new(Duration::from_secs(8));
    let first = leases.acquire(conn(1), "install-a", "tab-a", 1_000).unwrap();
    assert!(leases.authorize(conn(1), first.generation, 1_001).is_ok());
    assert!(matches!(leases.authorize(conn(2), first.generation, 1_001), Err(LeaseError::NotOwner)));
}

#[test]
fn foreground_interaction_reclaims_an_expired_lease_without_a_button() {
    let mut leases = WriterLeaseManager::new(Duration::from_secs(8));
    leases.acquire(conn(1), "phone", "pwa", 1_000).unwrap();
    leases.set_visibility(conn(1), false, 1_002);
    let desktop = leases.acquire(conn(2), "desktop", "browser", 1_003).unwrap();
    assert_eq!(desktop.owner_connection_id, 2);
}
```

- [ ] **Step 2: Run focused tests and verify failure**

Run: `cargo test remote::web::lease::tests --lib`  
Expected: compilation fails because `WriterLeaseManager` is absent.

- [ ] **Step 3: Implement the lease reducer and wire frames**

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeRequest {
    pub seen_runtime_instance_id: Option<String>,
    pub seen_revision: Option<u64>,
    pub route: String,
    pub desired_session_key: Option<StableSessionKey>,
    pub semantic_after_sequence: Option<u64>,
    pub client_instance_id: String,
    pub visible: bool,
    pub wants_writer_lease: bool,
}
```

Lease ownership is connection-scoped. Visibility and heartbeat renewals are explicit; hidden/disconnected owners become immediately preemptible and expiry is the crash/network-loss guarantee. Foreground interaction atomically transfers from a hidden/idle owner after only a sub-second active-input guard. The current native controller client ID is updated only after a web lease is authorized.

- [ ] **Step 4: Make resume one idempotent handshake**

The host compares runtime ID and revision, chooses full snapshot versus current projection, validates the stable route, returns the current journal cursor/bootstrap, and acquires a lease when allowed. A runtime mismatch is a hard reset response. Keep raw PTY bootstrap asynchronous so a spawning session cannot stall the handshake.

- [ ] **Step 5: Acknowledge semantic composer input**

`ComposerSubmit` contains `mutationId`, stable key, text, attachments, and lease generation. The bridge resolves the current PTY session, validates the lease, writes text plus the correct Enter sequence, emits the optimistic semantic UserMessage/Command once, and responds `ComposerAccepted`. Deduplicate mutation IDs for the current runtime and reject stale generation without writing.

- [ ] **Step 6: Run backend remote tests**

Run: `cargo test remote::web --lib`  
Expected: all remote web tests pass, including disconnect, same-cookie multi-tab, stale generation, and resume mismatch tests.

- [ ] **Step 7: Commit**

```powershell
git add src/remote
git commit -m "feat: make mobile resume and control automatic"
```

### Task 4: Move the browser client to the safe resumable protocol

**Files:**
- Modify: `web/src/api/types.ts`
- Modify: `web/src/api/ws.ts`
- Modify: `web/src/api/ws.test.ts`
- Modify: `web/src/store/index.ts`
- Modify: `web/src/store/index.test.ts`
- Modify: `web/package.json`

**Interfaces:**
- Consumes: Task 1-3 web frames.
- Produces: `resume()`, `submitComposer()`, semantic journal state keyed by stable session key, runtime-reset handling, and automatic lease state.

- [ ] **Step 1: Add the test script and failing protocol/reconciliation tests**

Add `"test": "vitest run"` to scripts and test these outcomes:

```ts
it("clears host-derived and draft state when runtime changes", () => {
  const state = createTestStore({ runtimeInstanceId: "old", journals: seededJournal() });
  state.getState().applySnapshot(snapshot({ runtimeInstanceId: "new", sessions: [] }));
  expect(state.getState().journals).toEqual({});
  expect(state.getState().activeSessionKey).toBeNull();
});

it("wakes with one resume frame instead of a send sequence", () => {
  const socket = fakeOpenSocket();
  createClient(socket).wake();
  expect(jsonFrames(socket)).toEqual([expect.objectContaining({ type: "resume" })]);
});

it("keeps composer text until ComposerAccepted", async () => {
  const promise = store.getState().submitComposer("tab:a", "hello");
  expect(store.getState().drafts["tab:a"]).toBe("hello");
  respondAccepted(lastMutationId());
  await promise;
  expect(store.getState().drafts["tab:a"]).toBe("");
});
```

- [ ] **Step 2: Run tests and verify failure**

Run: `npm --prefix web test -- src/api/ws.test.ts src/store/index.test.ts`  
Expected: tests fail against the legacy passthrough protocol.

- [ ] **Step 3: Replace TypeScript wire types and client behavior**

Mirror the allowlisted Rust types. Add a per-tab `clientInstanceId` in sessionStorage. `WsClient.wake()` always sends Resume after an open/reopen; it does not individually focus, subscribe, resize, and claim. Resolve/reject mutation promises by mutation ID and semantic cursor.

- [ ] **Step 4: Replace store truth with stable session keys and journals**

Keep `streamSessionId` only inside the raw-terminal slice. All routes, drafts, selected sessions, unread state, and semantic events use stable keys. On runtime mismatch, clear host-derived slices before applying the snapshot. Preserve the last visible route only long enough for Task 6 validation.

- [ ] **Step 5: Run web tests and typecheck**

Run: `npm --prefix web test`  
Expected: all Vitest tests pass.  
Run: `npm --prefix web run typecheck`  
Expected: both TypeScript projects pass.

- [ ] **Step 6: Commit**

```powershell
git add web/package.json web/src/api web/src/store
git commit -m "feat: reconcile browser state by host runtime"
```

### Task 5: Build native navigation, installed-only restore, and session home

**Files:**
- Create: `web/src/app/router.ts`
- Create: `web/src/app/router.test.ts`
- Create: `web/src/app/restore.ts`
- Create: `web/src/app/restore.test.ts`
- Create: `web/src/app/AppShell.tsx`
- Create: `web/src/platform/lifecycle.ts`
- Create: `web/src/sessions/sessionKey.ts`
- Create: `web/src/sessions/sessionModel.ts`
- Create: `web/src/sessions/sessionModel.test.ts`
- Create: `web/src/sessions/SessionsScreen.tsx`
- Create: `web/src/projects/ProjectsScreen.tsx`
- Create: `web/src/projects/ProjectScreen.tsx`
- Create: `web/src/settings/SettingsScreen.tsx`
- Modify: `web/src/App.tsx`
- Modify: `web/src/index.css`

**Interfaces:**
- Consumes: safe snapshots and stable-key store from Task 4.
- Produces: parsed `AppRoute`, History API navigation, installed-only route restoration, active/attention/recent grouping, and responsive native shell.

- [ ] **Step 1: Write failing route/restore/grouping tests**

```ts
expect(parseRoute("/session/tab/abc")).toEqual({ name: "session", kind: "tab", id: "abc" });
expect(resolveColdStart(rootRoute, saved, { standalone: false, snapshot })).toEqual(rootRoute);
expect(resolveColdStart(rootRoute, saved, { standalone: true, snapshot: newRuntime })).toEqual({ name: "sessions" });
expect(groupSessions(snapshot).active[0]).toMatchObject({ projectName: "DevManager" });
```

- [ ] **Step 2: Run focused tests and verify failure**

Run: `npm --prefix web test -- router.test.ts restore.test.ts sessionModel.test.ts`  
Expected: modules are missing.

- [ ] **Step 3: Implement one History API router and installed-only restore**

Routes are `/sessions`, `/projects`, `/projects/:projectId`, `/session/:kind/:stableId`, and `/settings`. A deep link always wins. Only standalone launch at `/` or `/sessions?source=pwa` may restore. Validate both runtime ID and stable key against the first fresh snapshot; otherwise replace with `/sessions`.

- [ ] **Step 4: Implement the native shell and session rows**

Use a three-item bottom tab bar under 768px and a sidebar above it. Each session row renders label, project name, state, relative activity, and attention count with a 44px minimum target. Sections are Needs attention, Active, and Recent; omit empty sections.

- [ ] **Step 5: Add the iPhone layout foundation**

```css
:root {
  --safe-top: env(safe-area-inset-top, 0px);
  --safe-right: env(safe-area-inset-right, 0px);
  --safe-bottom: env(safe-area-inset-bottom, 0px);
  --safe-left: env(safe-area-inset-left, 0px);
  font-family: ui-sans-serif, -apple-system, BlinkMacSystemFont, "SF Pro Text", sans-serif;
}

.app-shell {
  min-height: 100vh;
  min-height: 100dvh;
  padding-inline: var(--safe-left) var(--safe-right);
  overscroll-behavior: none;
}
```

Support system light/dark, reduced motion, visible keyboard focus, dynamic text wrapping, and no hover-only behavior.

- [ ] **Step 6: Run web tests, typecheck, and build**

Run: `npm --prefix web test`  
Run: `npm --prefix web run typecheck`  
Run: `npm --prefix web run build`  
Expected: all complete successfully.

- [ ] **Step 7: Commit**

```powershell
git add web/src
git commit -m "feat: add native mobile navigation and sessions home"
```

### Task 6: Build semantic session views, composer, and raw fallback

**Files:**
- Create: `web/src/drafts/draftStore.ts`
- Create: `web/src/drafts/draftStore.test.ts`
- Create: `web/src/sessions/Composer.tsx`
- Create: `web/src/sessions/Composer.test.tsx`
- Create: `web/src/sessions/SessionScreen.tsx`
- Create: `web/src/sessions/timeline/Timeline.tsx`
- Create: `web/src/sessions/timeline/eventRenderers.tsx`
- Create: `web/src/sessions/timeline/eventRenderers.test.tsx`
- Create: `web/src/sessions/views/AiSessionView.tsx`
- Create: `web/src/sessions/views/ServerSessionView.tsx`
- Create: `web/src/sessions/views/CommandSessionView.tsx`
- Create: `web/src/sessions/views/RawTerminalView.tsx`
- Modify: `web/src/components/Terminal.tsx`
- Modify: `web/src/index.css`
- Modify: `web/package.json`
- Modify: `web/package-lock.json`

**Interfaces:**
- Consumes: semantic journals, session summaries, acknowledged composer mutations, image-paste utility, and existing xterm bootstrap.
- Produces: calm/minimal/full DOM timeline, native composer/drafts, server controls, and automatic/manual raw-terminal mode.

- [ ] **Step 1: Write failing draft, density, and fallback tests**

Install the DOM component-test runtime with `npm --prefix web install --save-dev @testing-library/react@16.3.0 @testing-library/user-event@14.6.1 jsdom@26.1.0`. Mark DOM-oriented test files with `// @vitest-environment jsdom`.

```ts
it("scopes drafts to the current host runtime", () => {
  saveDraft("runtime-a", "tab:x", "hello");
  expect(loadDraft("runtime-a", "tab:x")).toBe("hello");
  clearOtherRuntimes("runtime-b");
  expect(loadDraft("runtime-a", "tab:x")).toBeNull();
});

it("calm density expands questions and collapses successful tools", () => {
  render(<EventRenderer density="calm" event={successfulTool} />);
  expect(screen.getByRole("button", { name: /tool/i })).toHaveAttribute("aria-expanded", "false");
  render(<EventRenderer density="calm" event={question} />);
  expect(screen.getByText(question.prompt)).toBeVisible();
});

expect(resolveViewMode({ adapterHealth: "healthy", ai: true, gridInteractionRequired: false, pinned: false })).toBe("semantic");
expect(resolveViewMode({ adapterHealth: "degraded", ai: true, gridInteractionRequired: false, pinned: false })).toBe("semantic");
expect(resolveViewMode({ adapterHealth: "degraded", ai: true, gridInteractionRequired: true, pinned: false })).toBe("terminal");
```

- [ ] **Step 2: Run focused tests and verify failure**

Run: `npm --prefix web test -- draftStore.test.ts Composer.test.tsx eventRenderers.test.tsx`  
Expected: modules are missing.

- [ ] **Step 3: Implement runtime-scoped bounded drafts and native composer**

Use localStorage with a versioned prefix, 32 KiB cap per draft, seven-day expiry, and only the current runtime ID. Flush on input and `pagehide`; delete after acknowledged submission or runtime change. The textarea uses `font-size: 16px`, grows to a capped height, exposes an accessible Send button, and uses `<input type="file" accept="image/png,image/jpeg" capture>` for AI attachments. Reuse the existing authenticated image-paste transport: validate PNG/JPEG and 5 MiB in the browser, wait for host acknowledgement, stage only beneath the selected session's existing `.devmanager/pasted-images` path, and retain its 24-hour cleanup.

- [ ] **Step 4: Implement semantic renderers and timeline anchoring**

Render all content through React text nodes. Calm mode expands errors/questions and collapses successful tools/diffs/output. Minimal hides detail events; full expands them. Preserve the user's scroll position unless they are already near the end, in which case new events follow automatically.

- [ ] **Step 5: Implement per-kind views**

AI gets conversational bubbles/cards and composer/interrupt. When its structured adapter is degraded, a native DOM screen/scrollback projector still supplies wrapping selectable text and the same composer. Server gets state/resources/port, start-stop-restart controls, and log events; when an existing server tab is in `interactive_shell` mode, it keeps `server:<commandId>` identity and renders command/output groups plus composer. SSH gets the same command view. Every navigation title includes the project subtitle.

- [ ] **Step 6: Keep xterm lazy and authoritative in raw mode**

Mount the existing `TerminalView` only in `RawTerminalView`. Automatic mode follows explicit grid/mouse interaction requirements, not provider adapter degradation; a session overflow option can pin/unpin terminal mode. Remove MobileKeyRow from semantic mode. Returning from raw requests a semantic bootstrap from the last cursor.

- [ ] **Step 7: Run web verification**

Run: `npm --prefix web test`  
Run: `npm --prefix web run typecheck`  
Run: `npm --prefix web run build`  
Expected: all pass.

- [ ] **Step 8: Commit**

```powershell
git add web/src
git commit -m "feat: render every session as a native mobile view"
```

### Task 7: Make the shell installable and the embedded build deterministic

**Files:**
- Modify: `web/package.json`
- Modify: `web/package-lock.json`
- Modify: `web/.gitignore`
- Modify: `web/.gitignore`
- Modify: `web/vite.config.ts`
- Modify: `web/index.html`
- Create: `web/src/pwa/register.ts`
- Create: `web/src/pwa/register.test.ts`
- Create: `web/src/sw.ts`
- Add: `web/public/icons/devmanager-180.png`
- Add: `web/public/icons/devmanager-192.png`
- Add: `web/public/icons/devmanager-512.png`
- Add: `web/public/icons/devmanager-maskable-512.png`
- Create: `examples/generate_pwa_icons.rs`
- Modify: `web/src/main.tsx`
- Modify: `src/remote/web/assets.rs`
- Modify: `build.rs`
- Modify: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: App shell and route state.
- Produces: manifest, custom service worker, safe update coordinator, cache headers, and a reproducible embedded bundle.

- [ ] **Step 1: Install PWA build dependency and write failing update tests**

Run: `npm --prefix web install --save-dev vite-plugin-pwa`  
Then test that an update activates only when there is no draft and no mutation:

```ts
expect(canActivateUpdate({ hasDraft: true, pendingMutations: 0 })).toBe(false);
expect(canActivateUpdate({ hasDraft: false, pendingMutations: 1 })).toBe(false);
expect(canActivateUpdate({ hasDraft: false, pendingMutations: 0 })).toBe(true);
```

- [ ] **Step 2: Implement injectManifest PWA configuration and custom worker**

Precache only the shell, hashed assets, and icons. Use NetworkOnly for `/api/**` and pairing; never cache authenticated snapshots or output. Handle push and notification clicks in the worker. Install updates in waiting state and activate automatically on a later safe cold/foreground navigation when composer/mutations are empty.

- [ ] **Step 3: Add the manifest and accessible iPhone metadata**

Manifest: ID/scope `/`, start `/sessions?source=pwa`, `display: standalone`, dark/light theme metadata, 192/512 any icons, and 512 maskable icon. Add Apple touch icon and `viewport-fit=cover`; remove `maximum-scale=1,user-scalable=no`.

- [ ] **Step 4: Generate icons from the existing DevManager 512px packaging source**

Create `examples/generate_pwa_icons.rs` using the existing `image` crate. Load `packaging/icons/devmanager-512.png`; save Lanczos3 `resize_exact` outputs at 180, 192, and 512; then resize to 410x410, overlay it at `(51, 51)` on a 512x512 `Rgba([9, 9, 11, 255])` canvas, and save the maskable output. Run `cargo run --example generate_pwa_icons`, then visually inspect all four files before committing.

- [ ] **Step 5: Add explicit cache/security headers**

Hashed assets use `public, max-age=31536000, immutable`; index, manifest, and service worker use `no-cache`. Add `X-Content-Type-Options: nosniff`, a frame denial policy, and a Content Security Policy compatible with the built app and WebSocket endpoint.

- [ ] **Step 6: Replace build.rs's placeholder heuristic**

Track the complete `web/bundle` output by removing its broad ignore rule. `npm run build` writes a source fingerprint into the bundle. `build.rs` performs no install, network access, or frontend build: it validates that tracked `index.html`, every referenced hashed asset, manifest, service worker, required icon, and fingerprint exist, and fails with `npm --prefix web ci && npm --prefix web run build` as the recovery command. Release/CI keeps the explicit `npm ci` plus build before Cargo packaging.

- [ ] **Step 7: Add a clean-bundle CI probe**

The job deletes `web/bundle`, runs the supported build, verifies referenced files, and exercises the embedded root plus `/session/tab/test` SPA fallback in a Rust route test.

- [ ] **Step 8: Verify and commit**

Run: `npm --prefix web test`  
Run: `npm --prefix web run build`  
Run: `cargo test remote::web::assets --lib`  
Expected: tests/build pass and generated bundle contains manifest/service worker/icons.

```powershell
git add web build.rs src/remote/web/assets.rs .github/workflows/release.yml
git commit -m "feat: ship an installable resilient DevManager PWA"
```

### Task 8: Add the Claude Code semantic hook adapter

**Files:**
- Create: `src/ai/mod.rs`
- Create: `src/ai/claude_hooks.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`
- Modify: `src/services/process_manager.rs`
- Modify: `src/remote/web/mod.rs`
- Modify: `src/remote/presentation.rs`
- Test: fixture-driven tests in `claude_hooks.rs`

**Interfaces:**
- Consumes: official Claude command-hook JSON and the semantic journal store.
- Produces: `ClaudeHookRegistry`, generated `--settings` file, early `claude-hook-relay` mode, and normalized AI events.

- [ ] **Step 1: Add representative sanitized fixtures and failing reducer tests**

```rust
#[test]
fn parallel_tools_reduce_by_tool_use_id() {
    let mut reducer = ClaudeReducer::new(key("tab:claude"));
    reducer.apply(fixture("pre_tool_a.json")).unwrap();
    reducer.apply(fixture("pre_tool_b.json")).unwrap();
    reducer.apply(fixture("post_tool_b.json")).unwrap();
    assert_eq!(reducer.tool("a").unwrap().state, SemanticToolState::Running);
    assert_eq!(reducer.tool("b").unwrap().state, SemanticToolState::Succeeded);
}

#[test]
fn relay_failure_is_always_fail_open() {
    assert_eq!(run_hook_relay(unreachable_endpoint(), b"{}"), ExitCode::SUCCESS);
}
```

- [ ] **Step 2: Run focused tests and verify failure**

Run: `cargo test ai::claude_hooks::tests --lib`  
Expected: module is missing.

- [ ] **Step 3: Implement the bounded registry and hook reducer**

Register a random nonce to one stable tab key and expiration. Accept only loopback relay posts with a matching nonce and a small body limit. Parse known common fields while tolerating unknown fields. Normalize SessionStart, prompts, displayed messages, tools, notifications/questions, Stop/Failure, and SessionEnd; never return a permission decision.

- [ ] **Step 4: Implement early relay mode before GPUI startup**

`devmanager claude-hook-relay --url <loopback> --nonce <value>` reads capped stdin, POSTs it with a short timeout, suppresses all output, and returns success regardless of delivery. Normal invocation follows the existing main path unchanged.

- [ ] **Step 5: Generate and inject session-scoped Claude settings**

For recognized Claude commands, merge only the hook configuration into a temp settings file and append `--settings <path>` with platform-correct quoting. Preserve custom/wrapper commands by falling back unchanged when safe injection cannot be proven. Record adapter health and clean the settings file/registry when the tab ends.

- [ ] **Step 6: Run coherent Rust verification and commit**

Run: `cargo test claude_hooks --lib`  
Run: `cargo test process_manager --lib`  
Expected: all Claude adapter and process-manager tests pass.

```powershell
git add src/ai src/main.rs src/services/process_manager.rs src/remote
git commit -m "feat: project Claude Code hooks into native sessions"
```

### Task 9: Add the transparent Codex app-server bridge

**Files:**
- Create: `src/ai/codex_bridge.rs`
- Modify: `src/ai/mod.rs`
- Modify: `src/services/process_manager.rs`
- Modify: `src/remote/presentation.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Test: protocol fixtures and loopback proxy integration tests in `codex_bridge.rs`

**Interfaces:**
- Consumes: exact resolved Codex executable, app-server JSONL, TUI WebSocket JSON-RPC.
- Produces: `CodexBridgeHandle`, remote endpoint, capability result, captured thread/turn/item IDs, and normalized events.

- [ ] **Step 1: Write failing transparent-forwarding and normalization tests**

```rust
#[tokio::test]
async fn unknown_json_rpc_round_trips_unchanged() {
    let (bridge, mut fake_server, mut fake_tui) = test_bridge().await;
    let raw = r#"{"method":"future/event","params":{"opaque":true}}"#;
    fake_server.send(raw).await;
    assert_eq!(fake_tui.recv().await.unwrap(), raw);
    bridge.shutdown().await;
}

#[test]
fn completed_item_replaces_streaming_delta() {
    let events = reduce_codex(fixtures(["agent_delta.json", "agent_completed.json"]));
    assert_eq!(events.assistant_message("item-1").text, "Final response");
    assert!(!events.assistant_message("item-1").streaming);
}
```

- [ ] **Step 2: Add only the networking/process features required by the proxy**

Add Tokio process support and `tokio-tungstenite` using a version compatible with existing Tokio/Rustls. Keep the loopback listener on `127.0.0.1` and reject non-loopback peers.

- [ ] **Step 3: Implement transparent forwarding off the semantic decode path**

Spawn `codex app-server --listen stdio://`, drain stderr, bridge newline-delimited JSON to one WebSocket connection, and forward unknown frames byte-for-byte. Copy frames to a bounded observer channel; observer failure/drop cannot block or terminate forwarding.

- [ ] **Step 4: Normalize documented stable v2 events**

Capture thread ID/session ID from responses. Map thread/turn status, agentMessage deltas/completion, command execution, file change/diff snapshots, tools, plans, approvals/user input, and errors. Treat item completion as authoritative and `turn/diff/updated` as a replacement snapshot.

- [ ] **Step 5: Integrate capability detection and fail-open launch**

Resolve one executable and version, probe `app-server` plus `--remote`, start the bridge, then launch the normal PTY TUI with the endpoint. If any preflight/initialization step fails, kill only the sidecar and launch the original command unchanged with adapter health `degraded`. Store the bridge handle by tab/session and tear it down with the process tree.

- [ ] **Step 6: Run targeted and combined adapter tests**

Run: `cargo test codex_bridge --lib`  
Run: `cargo test ai:: --lib`  
Expected: forwarding, normalization, fallback, teardown, and combined adapter tests pass.

- [ ] **Step 7: Commit**

```powershell
git add Cargo.toml Cargo.lock src/ai src/services/process_manager.rs src/remote/presentation.rs
git commit -m "feat: project Codex app-server events into native sessions"
```

### Task 10: Add actionable Web Push, badges, and deep links

**Files:**
- Create: `src/remote/web/push.rs`
- Modify: `src/remote/web/mod.rs`
- Modify: `src/remote/web/dto.rs`
- Modify: `src/remote/presentation.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `web/src/pwa/notifications.ts`
- Create: `web/src/pwa/notifications.test.ts`
- Modify: `web/src/sw.ts`
- Modify: `web/src/settings/SettingsScreen.tsx`
- Modify: `web/src/store/index.ts`

**Interfaces:**
- Consumes: host attention transitions and authenticated PushSubscription payloads.
- Produces: persisted host VAPID/subscription configuration, redacted pushes, notification click routing, and badge count.

- [ ] **Step 1: Write failing subscription/auth/payload tests**

```rust
#[test]
fn push_payload_contains_no_prompt_or_terminal_content() {
    let payload = PushPayload::attention(&summary_fixture(), AttentionKind::NeedsInput);
    let json = serde_json::to_string(&payload).unwrap();
    assert!(!json.contains("PROMPT_SENTINEL"));
    assert!(!json.contains("OUTPUT_SENTINEL"));
    assert_eq!(payload.route, "/session/tab/tab-1");
}
```

```ts
it("opens the stable deep link and applies the aggregate badge", async () => {
  await handlePush(push({ route: "/session/tab/tab-1", badge: 2 }), swClients);
  expect(swClients.openWindow).toHaveBeenCalledWith("/session/tab/tab-1");
  expect(navigator.setAppBadge).toHaveBeenCalledWith(2);
});
```

- [ ] **Step 2: Add the pinned Web Push request builder**

Add `web-push-native = "0.4.0"`, whose documented builder implements RFC 8030 payload encryption/VAPID and produces an HTTP request. Execute that request through the existing `ureq` TLS client. Generate VAPID material once and persist it as host remote-web secret configuration; never send the private key to the browser.

- [ ] **Step 3: Add authenticated subscribe/unsubscribe/public-key endpoints**

Validate endpoint URL scheme and P-256/auth key sizes, cap request bodies, associate subscriptions with the paired browser install, and preserve them across host runtime restarts. Delete subscriptions on terminal gone/not-found push-service responses.

- [ ] **Step 4: Emit only actionable attention transitions**

Send Claude/Codex needs-input/completed, server-crashed, and unexpected-SSH-disconnect notifications only when the target client is not visibly viewing the session. Payload contains generic title, project/session label, action kind, runtime ID, stable route, event ID, and aggregate badge—never prompt/code/log text.

- [ ] **Step 5: Implement the explicit permission gesture and service-worker handlers**

Settings shows Enable notifications only in a secure standalone context with Push support. Request permission from that click, subscribe with the host public key, and POST it authenticated. `push` always displays a notification; `notificationclick` focuses/navigates an existing client or opens the route. A host attention acknowledgement clears the badge.

- [ ] **Step 6: Run notification and security tests**

Run: `cargo test remote::web --lib`  
Run: `npm --prefix web test -- notifications.test.ts`  
Expected: all pass.

- [ ] **Step 7: Commit**

```powershell
git add Cargo.toml Cargo.lock src/remote web/src
git commit -m "feat: add actionable mobile notifications"
```

### Task 11: Complete integration, accessibility, and real mobile validation

**Files:**
- Modify: `web/src/store/index.test.ts`
- Modify: `web/src/app/restore.test.ts`
- Create: `web/src/sessions/views/viewMode.test.ts`
- Modify: `src/remote/web/bridge.rs`
- Modify: `src/remote/web/wire.rs`
- Modify: `src/remote/presentation.rs`
- Modify: `README.md`
- Create: `docs/REMOTE_MOBILE_WEB.md`

**Interfaces:**
- Consumes: all previous tasks.
- Produces: verified coordinated release behavior and operator documentation.

- [ ] **Step 1: Add end-to-end store/protocol integration fixtures**

Cover warm resume, changed runtime, deleted restored tab, sequence rollover, session removal while backgrounded, adapter degradation, raw-mode enter/exit, and update deferral with a non-empty draft.

- [ ] **Step 2: Run format, lint, unit, type, and build verification**

Run: `cargo fmt --check`  
Expected: no diff.  
Run: `cargo test --lib`  
Expected: all Rust library tests pass.  
Run: `npm --prefix web test`  
Expected: all Vitest tests pass.  
Run: `npm --prefix web run typecheck`  
Expected: no TypeScript errors.  
Run: `npm --prefix web run build`  
Expected: PWA bundle succeeds and required artifacts exist.

- [ ] **Step 3: Run clean embedded-build verification**

In a disposable copy/worktree, remove generated `web/bundle` contents, run `cargo build`, and verify that the embedded root, manifest, service worker, icons, referenced assets, and a deep-link fallback all return successful responses with correct cache headers.

- [ ] **Step 4: Validate visually at iPhone and desktop sizes**

Use a paired local test host or a sanitized demo fixture. At 390x844 and 430x932, inspect Sessions, Projects, Settings, each session kind, keyboard-open composer, image attachment, terminal fallback, light/dark, large text, safe-area landscape, offline/reconnecting, and attention cards. Capture screenshots for review evidence.

- [ ] **Step 5: Exercise lifecycle behavior against a live host**

Open a session, background/foreground, close/reopen the PWA, continue from another browser, drop/recover networking, and restart the native host. Confirm no buttons are needed, host activity is current, drafts survive only the same runtime, and host restart yields a blank runtime.

- [ ] **Step 6: Exercise secure-context capabilities**

Through a stable HTTPS origin, install to an iPhone Home Screen, enable notifications by gesture, trigger each actionable class, verify deep links and badge clearing, and record any platform-specific limitation. Plain HTTP diagnostics must accurately report unavailable features.

- [ ] **Step 7: Inspect the live browser wire for secret absence**

Use DevTools/network capture and sentinel fixtures to confirm browser JSON never contains passwords, private keys, GitHub tokens, environment values, startup commands, or unrelated provider sessions.

- [ ] **Step 8: Document operation and limitations**

Document HTTPS/tunnel requirements, install steps, notification permission, host-authority/restart behavior, automatic lease semantics, provider adapter fallback, and raw terminal behavior. Do not imply that LAN HTTP can install or push.

- [ ] **Step 9: Commit**

```powershell
git add README.md docs web/src src
git commit -m "docs: verify and document native mobile web"
```

---

## Final verification gate

- [ ] Run `git diff --check` and confirm no whitespace errors.
- [ ] Run `git status --short` and account for every changed/generated file.
- [ ] Run the complete Rust and web verification commands from Task 11 once more after the last fix.
- [ ] Review the implementation against every success criterion and non-goal in `docs/superpowers/specs/2026-07-13-native-mobile-web-interface-design.md`.
- [ ] Request an independent code review and resolve all correctness/security findings before declaring completion.
