# Native Session Reconnect Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep native session screens connected and native-first by separating semantic focus from raw PTY subscriptions and eliminating redundant bootstrap replay payloads.

**Architecture:** Extend atomic Resume with optional `rawSessionId`; semantic focus continues to use `desiredSessionKey`, while only the mounted raw view supplies the PTY ID. Encode the authoritative screen snapshot without replay bytes and treat Claude/Codex terminal modes as semantic-capable unless the user manually selects raw.

**Tech Stack:** Rust, Axum WebSockets, Serde, React 18, Zustand, TypeScript, Vitest, Cargo test.

## Global Constraints

- Native session screens never subscribe to PTY output.
- Raw terminal subscription is automatic on mount/unmount and survives reconnect through atomic Resume.
- No extra resume, reconnect, or confirmation buttons.
- Claude and Codex are native-first; raw remains an explicit user option.
- Existing non-AI terminal-mode behavior remains unchanged.
- Full Rust tests run with `--test-threads=1` because the baseline profile fixture is process-global.

---

### Task 1: Atomic raw-stream resume contract

**Files:**
- Modify: `src/remote/web/wire.rs`
- Modify: `src/remote/web/bridge.rs`
- Modify: `src/remote/web/dto.rs`
- Modify: `web/src/api/types.ts`
- Modify: `web/src/api/ws.ts`
- Test: `src/remote/web/wire.rs`
- Test: `src/remote/web/bridge.rs`
- Test: `web/src/api/ws.test.ts`

**Interfaces:**
- Consumes: existing `ResumeRequest.desired_session_key` semantic focus.
- Produces: `ResumeRequest.raw_session_id: Option<String>` and `ResumeContext.rawSessionId: string | null`.

- [ ] **Step 1: Write failing Rust wire and bridge tests**

Add a wire test that deserializes `rawSessionId`, plus bridge tests asserting a semantic-only resume leaves `subscribed_session_ids` empty and a raw resume subscribes only the requested PTY session.

```rust
assert_eq!(request.raw_session_id.as_deref(), Some("pty-a"));
assert!(semantic_client.subscribed_session_ids.is_empty());
assert_eq!(raw_client.subscribed_session_ids, HashSet::from(["pty-a".to_string()]));
```

- [ ] **Step 2: Run the focused Rust tests and verify RED**

Run: `$env:CARGO_TARGET_DIR='C:\Code\userfirst\devmanager\target'; cargo test remote::web::wire::tests::resume -- --test-threads=1 && cargo test remote::web::bridge::tests::resume -- --test-threads=1`

Expected: compilation or assertion failure because `raw_session_id` does not exist and Resume still derives raw subscription from semantic focus.

- [ ] **Step 3: Add the optional wire field and split bridge subscription state**

```rust
#[serde(default)]
pub raw_session_id: Option<String>,
```

Validate it with `valid_session_id`, then set `subscribed_session_ids`, `bootstrap_pending_session_ids`, and `focused_session_id` only from `raw_session_id`. Continue using `desired_session_key` for semantic replay and browser attention acknowledgement.

- [ ] **Step 4: Add a failing TypeScript Resume serialization test**

```ts
expect(sentResume).toMatchObject({
  desiredSessionKey: "tab:a",
  rawSessionId: "pty-a",
});
```

Run: `npm test -- src/api/ws.test.ts`

Expected: FAIL because `ResumeContext` has no `rawSessionId`.

- [ ] **Step 5: Add `rawSessionId` to the TypeScript contract and default Resume**

```ts
export interface ResumeContext {
  // existing fields
  rawSessionId: string | null;
}
```

Set `rawSessionId: null` in `defaultResumeContext`; `WsClient.resume()` continues spreading the callback's complete atomic context.

- [ ] **Step 6: Run focused Rust and web tests and verify GREEN**

Run: `$env:CARGO_TARGET_DIR='C:\Code\userfirst\devmanager\target'; cargo test remote::web::wire::tests::resume -- --test-threads=1; cargo test remote::web::bridge::tests::resume -- --test-threads=1`

Run: `npm test -- src/api/ws.test.ts`

Expected: all focused tests pass.

### Task 2: Raw view mount owns PTY subscription

**Files:**
- Modify: `web/src/store/index.ts`
- Modify: `web/src/store/index.test.ts`
- Modify: `web/src/sessions/views/RawTerminalView.tsx`
- Test: `web/src/sessions/SessionScreen.test.tsx`

**Interfaces:**
- Consumes: `ResumeContext.rawSessionId` from Task 1.
- Produces: `setRawTerminalSession(sessionId: string | null): void` and mount-scoped raw ownership.

- [ ] **Step 1: Rewrite the store regression test for native selection**

```ts
useStore.getState().setActiveSession("pty-a");
expect(useStore.getState().rawTerminal.activeStreamSessionId).toBeNull();
expect(client?.callbacks.getResumeContext?.()).toMatchObject({
  desiredSessionKey: "tab:a",
  rawSessionId: null,
});
```

Add a second test that calls `setRawTerminalSession("pty-a")`, expects one wake and `rawSessionId: "pty-a"`, then clears it and expects `rawSessionId: null`.

- [ ] **Step 2: Run store tests and verify RED**

Run: `npm test -- src/store/index.test.ts`

Expected: FAIL because session selection still populates the raw stream and no raw-specific setter exists.

- [ ] **Step 3: Separate semantic selection from raw state**

Remove every assignment that derives `activeStreamSessionId` from `activeSessionKey`, including snapshot reconciliation, resume handling, AI launch/open flows, and `setActiveSession`. Add:

```ts
setRawTerminalSession(sessionId) {
  if (get().rawTerminal.activeStreamSessionId === sessionId) return;
  set((state) => ({
    rawTerminal: { ...state.rawTerminal, activeStreamSessionId: sessionId },
  }));
  get().client?.wake();
}
```

Return `rawSessionId: state.rawTerminal.activeStreamSessionId` from `getResumeContext`.

- [ ] **Step 4: Make `RawTerminalView` acquire and release raw ownership**

```tsx
const setRawTerminalSession = useStore((state) => state.setRawTerminalSession);
useEffect(() => {
  setRawTerminalSession(sessionId);
  return () => setRawTerminalSession(null);
}, [sessionId, setRawTerminalSession]);
```

- [ ] **Step 5: Run store and session tests and verify GREEN**

Run: `npm test -- src/store/index.test.ts src/sessions/SessionScreen.test.tsx`

Expected: all focused tests pass and no legacy subscribe/focus frame is emitted.

### Task 3: Native-first AI terminal classification

**Files:**
- Modify: `src/remote/presentation.rs`
- Test: `src/remote/presentation.rs`

**Interfaces:**
- Consumes: `SemanticSource::{Claude,Codex}` and `TerminalModeSnapshot`.
- Produces: AI metadata with `raw_required = false`; non-AI behavior unchanged.

- [ ] **Step 1: Change the existing presentation test to the desired AI behavior**

```rust
store.observe_native_terminal_mode(
    "ai-runtime",
    TerminalModeSnapshot { mouse_report_click: true, ..alternate_screen },
    102,
);
assert!(!store.metadata(&StableSessionKey::from_tab("ai-tab")).unwrap().raw_required);
```

Also assert a shell with mouse reporting remains raw.

- [ ] **Step 2: Run the focused test and verify RED**

Run: `$env:CARGO_TARGET_DIR='C:\Code\userfirst\devmanager\target'; cargo test remote::presentation::tests::native_terminal_mode_keeps_ai_alternate_screens_semantic_but_shells_raw -- --exact --test-threads=1`

Expected: FAIL because AI mouse reporting currently sets `raw_required = true`.

- [ ] **Step 3: Keep AI modes semantic**

```rust
let raw_required = if matches!(source, SemanticSource::Claude | SemanticSource::Codex) {
    false
} else {
    mode.alternate_screen || mode.mouse_reporting()
};
```

- [ ] **Step 4: Run the focused test and verify GREEN**

Run the exact command from Step 2.

Expected: PASS.

### Task 4: Compact raw terminal bootstrap

**Files:**
- Modify: `src/remote/web/bridge.rs`
- Test: `src/remote/web/bridge.rs`

**Interfaces:**
- Consumes: `RemoteSessionBootstrap { screen, replay_bytes }`.
- Produces: `sessionBootstrap.replayBase64 == ""` when `screen.rows > 0 && screen.cols > 0`.

- [ ] **Step 1: Add failing encoding tests**

Extend `encode_outbound_bootstrap_carries_screen_snapshot` to assert a valid screen omits replay, and add a no-screen case that retains base64 fallback bytes.

```rust
assert_eq!(value["replayBase64"], "");
assert_eq!(fallback["replayBase64"], "Ym9vdA==");
```

- [ ] **Step 2: Run the encoding tests and verify RED**

Run: `$env:CARGO_TARGET_DIR='C:\Code\userfirst\devmanager\target'; cargo test remote::web::bridge::tests::encode_outbound_bootstrap -- --test-threads=1`

Expected: FAIL because replay bytes are always base64 encoded.

- [ ] **Step 3: Encode replay only as a fallback**

```rust
let replay_base64 = if bootstrap.screen.rows > 0 && bootstrap.screen.cols > 0 {
    String::new()
} else {
    STANDARD.encode(&bootstrap.replay_bytes)
};
```

- [ ] **Step 4: Run encoding tests and verify GREEN**

Run the command from Step 2.

Expected: PASS.

### Task 5: Protocol version, documentation, and complete verification

**Files:**
- Modify: `src/remote/web/dto.rs`
- Modify: `web/src/api/types.ts`
- Modify: `docs/superpowers/specs/2026-07-14-native-session-reconnect-design.md` only if implementation discoveries require clarification

**Interfaces:**
- Consumes: completed tasks 1-4.
- Produces: matching host/client protocol constants and release-ready verification evidence.

- [ ] **Step 1: Increment matching protocol constants and update exact-value tests**

Increment Rust `WEB_PROTOCOL_VERSION` and TypeScript `WEB_PROTOCOL_VERSION` together. Update tests that assert the numeric value.

- [ ] **Step 2: Run formatting and focused suites**

Run: `cargo fmt --all -- --check`

Run: `npm test`

Run: `npm run typecheck`

Expected: formatting clean, 0 web test failures, 0 TypeScript errors.

- [ ] **Step 3: Run full Rust verification serially**

Run: `$env:CARGO_TARGET_DIR='C:\Code\userfirst\devmanager\target'; cargo test -- --test-threads=1`

Run: `$env:CARGO_TARGET_DIR='C:\Code\userfirst\devmanager\target'; cargo clippy --all-targets --all-features -- -D warnings`

Expected: all Rust unit/integration/doc tests pass; Clippy exits 0.

- [ ] **Step 4: Build the release artifacts**

Run: `npm run build` in `web/`.

Run: `$env:CARGO_TARGET_DIR='C:\Code\userfirst\devmanager\target'; cargo build --release`

Expected: both builds exit 0.

- [ ] **Step 5: Install and live-test the replacement build**

Stop the installed DevManager only when the release binary is ready, replace the installed executable through the repository's documented installer/release path, restart it, and verify in the in-app browser:

- Sessions remains Connected.
- Claude opens as a native transcript and remains Connected for at least two heartbeat intervals.
- The native server screen remains Connected.
- The composer accepts text input without a manual reconnect.
- “Use raw terminal” mounts the terminal, remains Connected, and switching back to native stops raw mode without a button to resume.
- Returning to the tab restores the same route automatically.

- [ ] **Step 6: Review diff and integrate**

Run: `git diff --check`, `git status --short`, and `git diff --stat`.

Commit the verified change on `codex/native-session-reconnect`. Merge to `master` only after verification and remove the worktree after integration.
