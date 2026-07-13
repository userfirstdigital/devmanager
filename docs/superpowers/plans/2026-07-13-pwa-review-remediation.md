# PWA Review Remediation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix simultaneous hello incompatibility recovery, preserve exact drafts through forced compatible-bundle reloads, and prevent stale Composer attachment safety after scope exit.

**Architecture:** Build mismatch takes precedence during the mandatory first hello while every incompatible socket still stops before protocol-ready traffic. Build recovery stages an exact, verified, bounded session handoff and exposes a transient Zustand safety flag so the existing worker gates can proceed without weakening ordinary update policy. Composer uses its existing generation cancellation mechanism on unmount.

**Tech Stack:** TypeScript, React 18, Zustand, Vitest, Vite PWA, Rust embedded web bundle.

## Global Constraints

- Work only in `.worktrees/native-mobile-web-task7`.
- Write each regression test first and observe the expected failure before production edits.
- Ordinary service-worker updates remain blocked by any raw non-empty draft.
- A draft is recoverable only after exact sessionStorage write/read verification for the same runtime.
- Pending mutations, attachments, and attachment reads remain blocking.
- Do not send WebSocket resume, lease, or mutation traffic for any incompatible hello.

---

### Task 1: Simultaneous hello mismatch recovery

**Files:**
- Modify: `web/src/api/ws.test.ts`
- Modify: `web/src/api/ws.ts`

**Interfaces:**
- Consumes: first-frame `hello { protocolVersion, webBuildId }`.
- Produces: `WsHelloFailure.buildMismatch` whenever `webBuildId` differs; equal-build protocol mismatches still produce `protocolMismatch`.

- [ ] **Step 1: Write the failing simultaneous-mismatch test**

Add a test that opens the fake transport without its automatic hello, emits a hello with both `WEB_PROTOCOL_VERSION + 1` and `webBuildId: "different-host-build"`, and asserts:

```ts
expect(onHelloFailure).toHaveBeenCalledWith({
  kind: "buildMismatch",
  expectedBuildId: CLIENT_WEB_BUILD_ID,
  receivedBuildId: "different-host-build",
});
expect(jsonFrames(socket)).toEqual([]);
expect(socket.readyState).toBe(FakeWebSocket.CLOSED);
```

- [ ] **Step 2: Verify RED**

Run: `npm --prefix web test -- src/api/ws.test.ts`

Expected: the new test receives `protocolMismatch`, proving build recovery is bypassed.

- [ ] **Step 3: Implement build-first incompatibility precedence**

In the pre-ready hello branch of `WsClient.onmessage`, compare `webBuildId` before `protocolVersion`. Keep both checks before `completeHello` and retain the existing `failHello` stop behavior.

- [ ] **Step 4: Verify GREEN**

Run: `npm --prefix web test -- src/api/ws.test.ts`

Expected: all WebSocket tests pass.

### Task 2: Exact runtime-scoped draft handoff

**Files:**
- Modify: `web/src/drafts/draftStore.test.ts`
- Modify: `web/src/drafts/draftStore.ts`
- Modify: `web/src/store/index.test.ts`
- Modify: `web/src/store/index.ts`
- Modify: `web/src/pwa/storeSafety.test.ts`
- Modify: `web/src/pwa/storeSafety.ts`

**Interfaces:**
- Produces: `stageDraftHandoff(runtimeInstanceId, drafts): boolean`.
- Produces: `hasExactDraftHandoff(runtimeInstanceId, drafts): boolean`.
- Produces: Zustand `compatibleDraftHandoffReady: boolean`.
- Consumes: existing `SessionScreen` call to `loadDraft`, which performs one-time handoff restoration without changing the component API.

- [ ] **Step 1: Write failing draft-store tests**

Clear both local and session storage in setup. Save and stage `"  exact draft\n"`, then assert staging succeeds, `hasExactDraftHandoff` is true, `loadDraft` restores the exact string, and `hasExactDraftHandoff` becomes false. Also assert staging returns false for an over-32-KiB draft and when sessionStorage `setItem` throws.

- [ ] **Step 2: Verify draft-store RED**

Run: `npm --prefix web test -- src/drafts/draftStore.test.ts`

Expected: imports fail because the handoff APIs do not exist.

- [ ] **Step 3: Implement bounded verified handoff storage**

Add a separate versioned session key. Filter empty drafts, reject any draft above `MAX_DRAFT_BYTES`, reject a serialized payload above 512 KiB, write to `sessionStorage`, then read back and compare runtime plus every exact draft. `loadDraft` consumes one matching handoff entry before falling back to local storage. Extend `removeDraft`, `clearOtherRuntimes`, and `pruneDrafts` to remove matching handoff entries.

- [ ] **Step 4: Verify draft-store GREEN**

Run: `npm --prefix web test -- src/drafts/draftStore.test.ts`

Expected: all draft-store tests pass.

- [ ] **Step 5: Write failing store and PWA safety tests**

In the store harness, mock `stageDraftHandoff`. For a build mismatch with runtime `runtime-a` and draft `tab:a`, assert it stages the exact record before `requestCompatibleBuild`. When staging returns false, assert recovery is not requested and the error states that the draft could not be preserved.

In `storeSafety.test.ts`, set an actual draft and assert it remains unsafe until `compatibleDraftHandoffReady` is true; pending mutations and attachment state remain unsafe even when that flag is true.

- [ ] **Step 6: Verify store/safety RED**

Run: `npm --prefix web test -- src/store/index.test.ts src/pwa/storeSafety.test.ts`

Expected: no handoff is staged and the safety reader still reports the staged draft as unsafe.

- [ ] **Step 7: Integrate handoff state with build recovery**

Add `compatibleDraftHandoffReady` to store initial/reset state and clear it whenever draft text or runtime identity changes. During `buildMismatch`, stage all current drafts for the current runtime. Only on success set the flag, request the compatible build, and show the automatic reconciliation message. On failure remain closed, do not request recovery, and report that exact draft preservation was unavailable.

Update `readStoreUpdateSafetyState` so `hasDraft` is false only when `compatibleDraftHandoffReady === true`. Do not change pending-mutation or attachment counts.

- [ ] **Step 8: Verify store/safety GREEN and focused recovery**

Run: `npm --prefix web test -- src/store/index.test.ts src/pwa/storeSafety.test.ts src/pwa/register.test.ts`

Expected: build recovery can proceed with a staged draft, normal draft gating remains blocked, and all focused tests pass.

### Task 3: Cancel attachment reads on scope exit

**Files:**
- Modify: `web/src/sessions/Composer.test.tsx`
- Modify: `web/src/sessions/Composer.tsx`

**Interfaces:**
- Consumes: existing `scopeGenerationRef` and `onSafetyStateChange` callback.
- Produces: one final `{ selectedAttachments: 0, attachmentLoads: 0 }` publication on unmount; no continuation can publish afterward.

- [ ] **Step 1: Write failing deferred-read tests**

Override a real `File.arrayBuffer` with an unresolved promise. Start upload using `fireEvent.change`, wait for `attachmentLoads: 1`, then separately rerender with a different `scopeKey` or unmount. Resolve the read and assert the last safety state remains `{ selectedAttachments: 0, attachmentLoads: 0 }` in both cases.

- [ ] **Step 2: Verify Composer RED**

Run: `npm --prefix web test -- src/sessions/Composer.test.tsx`

Expected: the unmount case never publishes zero or republishes stale loading safety.

- [ ] **Step 3: Implement generation invalidation cleanup**

Keep the latest safety callback in a ref. Add an empty-dependency unmount effect that increments `scopeGenerationRef`, clears the loading and attachment refs, and publishes zero through that callback. Existing async generation checks reject late completion.

- [ ] **Step 4: Verify Composer GREEN**

Run: `npm --prefix web test -- src/sessions/Composer.test.tsx`

Expected: both deferred-read lifecycle tests and all existing Composer tests pass.

### Task 4: Regenerate and verify

**Files:**
- Regenerate: `web/bundle/**`

- [ ] **Step 1: Run source gates**

Run `npm --prefix web ci --no-audit --no-fund`, `npm --prefix web test`, `npm --prefix web run typecheck`, and `npm --prefix web audit --omit=dev`.

Expected: all commands exit zero and audit reports zero vulnerabilities.

- [ ] **Step 2: Build twice and compare bytes**

Run `npm --prefix web run build`, hash every file under `web/bundle`, run the same build again, and compare the file/path SHA-256 lists.

Expected: the lists are identical and the source fingerprint is stable.

- [ ] **Step 3: Run Rust and repository gates**

Run `cargo test --lib remote::web:: -j 1`, `rustfmt --edition 2021 --check build.rs src/remote/web/assets.rs src/remote/web/bridge.rs src/remote/web/mod.rs`, and `git diff --check`.

Expected: all 136+ scoped Rust tests pass, touched Rust files are formatted, and diff check is clean.

- [ ] **Step 4: Commit remediation**

Stage only intended source, tests, docs, and generated bundle files. Commit with `fix(web): preserve drafts through compatibility reloads`.

Report the commit SHA, RED/GREEN evidence, test counts, bundle fingerprint, determinism count, and clean worktree status.
