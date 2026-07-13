# Build-Bound PWA Draft Handoff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bind forced-update draft handoffs to the requested host build and return handoff text only after verified one-time storage consumption.

**Architecture:** Add `targetBuildId` to the versioned handoff and use the running bundle's compiled `CLIENT_WEB_BUILD_ID` as the consumption authority. Replace the transient readiness boolean with the target build ID, and use an internal tri-state consume result so storage-integrity failure cannot fall through to ordinary local draft loading.

**Tech Stack:** TypeScript, Zustand, Vitest/jsdom, Vite PWA, Rust embedded web bundle.

## Global Constraints

- Work only in `.worktrees/native-mobile-web-task7`.
- Write every regression test first and observe its expected failure.
- Ordinary service-worker updates remain blocked by every non-empty draft.
- The old or any wrong bundle may verify a target-build handoff for recovery safety but may not consume or mutate it.
- Only `CLIENT_WEB_BUILD_ID === targetBuildId` may consume.
- Removal and rewrite must be read-back verified before returning text.
- Pending mutations, selected attachments, and attachment reads remain blocking.

---

### Task 1: Bind staging, safety, and consumption to the target build

**Files:**
- Modify: `web/src/drafts/draftStore.test.ts`
- Modify: `web/src/drafts/draftStore.ts`
- Modify: `web/src/pwa/storeSafety.test.ts`
- Modify: `web/src/pwa/storeSafety.ts`
- Modify: `web/src/store/index.test.ts`
- Modify: `web/src/store/index.ts`

**Interfaces:**
- Produces: `stageDraftHandoff(targetBuildId, runtimeInstanceId, drafts): boolean`.
- Produces: `hasExactDraftHandoff(targetBuildId, runtimeInstanceId, drafts): boolean`.
- Produces: Zustand `compatibleDraftHandoffTargetBuildId: string | null`.
- Consumes: `CLIENT_WEB_BUILD_ID` inside `loadDraft` to authorize one-time handoff consumption.

- [ ] **Step 1: Write failing build-binding and remount tests**

Import `CLIENT_WEB_BUILD_ID`. Update staging calls to include a target. Add tests with a distinct `futureBuildId` that assert:

```ts
saveDraft("runtime-a", "tab:x", exactDraft);
expect(stageDraftHandoff(futureBuildId, "runtime-a", { "tab:x": exactDraft })).toBe(true);
expect(loadDraft("runtime-a", "tab:x")).toBe(exactDraft);
expect(hasExactDraftHandoff(futureBuildId, "runtime-a", { "tab:x": exactDraft })).toBe(true);
```

This proves an old-build remount may use local persistence without consuming the target-build handoff. Also stage for `CLIENT_WEB_BUILD_ID` without a local draft and assert the first load returns exact text, the handoff no longer matches, and the second load returns `null`.

- [ ] **Step 2: Verify build-binding RED**

Run: `npm --prefix web test -- src/drafts/draftStore.test.ts`

Expected: wrong-build loading consumes the handoff, so exact handoff readiness becomes false; the new target-aware signatures are not implemented.

- [ ] **Step 3: Write failing store and safety target tests**

Change the draft-store mock and assertions so a build mismatch stages:

```ts
stageDraftHandoff("different-host-build", "runtime-1", {
  "tab:a": "  preserve exactly\n",
});
```

Assert `compatibleDraftHandoffTargetBuildId === "different-host-build"`, and assert draft edits/reset paths restore it to `null`. In the real PWA safety test, stage a target-build handoff and expose the same target in state; mismatched target IDs must remain unsafe.

- [ ] **Step 4: Verify store/safety RED**

Run: `npm --prefix web test -- src/store/index.test.ts src/pwa/storeSafety.test.ts`

Expected: store staging lacks the received build argument and safety cannot validate a target build.

- [ ] **Step 5: Implement target-build binding**

Add `targetBuildId: string` to `StoredDraftHandoff`. Require non-empty target/runtime IDs when staging. Include target equality in exact matching. Import `CLIENT_WEB_BUILD_ID` and let `takeDraftHandoff` return no match without writing unless both runtime and target equal the running bundle.

Replace `compatibleDraftHandoffReady` with `compatibleDraftHandoffTargetBuildId`. Set it to the received build only after exact staging, clear it on all existing draft/runtime reset paths, and pass it to `hasExactDraftHandoff` in the PWA safety reader.

- [ ] **Step 6: Verify target-build GREEN**

Run: `npm --prefix web test -- src/drafts/draftStore.test.ts src/store/index.test.ts src/pwa/storeSafety.test.ts src/pwa/register.test.ts`

Expected: remount, wrong-build, exact target safety, matching-build consumption, and existing recovery tests pass.

### Task 2: Verify one-time removal and rewrite before returning text

**Files:**
- Modify: `web/src/drafts/draftStore.test.ts`
- Modify: `web/src/drafts/draftStore.ts`

**Interfaces:**
- Internal result: `{ kind: "missing" } | { kind: "consumed"; text: string } | { kind: "integrityFailure" }`.
- `loadDraft` falls back to local storage only for `missing`; it returns `null` for `integrityFailure`.

- [ ] **Step 1: Write failing removal-integrity test**

Stage one matching-build draft, replace `Storage.prototype.removeItem` with a no-op, then assert:

```ts
expect(loadDraft("runtime-a", "tab:x")).toBeNull();
expect(hasExactDraftHandoff(CLIENT_WEB_BUILD_ID, "runtime-a", { "tab:x": exactDraft })).toBe(true);
```

- [ ] **Step 2: Verify removal RED**

Run: `npm --prefix web test -- src/drafts/draftStore.test.ts`

Expected: `loadDraft` returns the text even though storage still contains the handoff.

- [ ] **Step 3: Implement verified removal and tri-state consumption**

After `removeItem`, require `getItem(HANDOFF_STORAGE_KEY) === null`. Make `takeDraftHandoff` return `integrityFailure` when the verified write returns false. In `loadDraft`, return consumed text, return `null` for integrity failure, and use local storage only for missing/nonmatching handoffs.

- [ ] **Step 4: Verify removal GREEN**

Run: `npm --prefix web test -- src/drafts/draftStore.test.ts`

Expected: the removal failure returns no text and the original handoff remains.

- [ ] **Step 5: Write failing rewrite-integrity test**

Stage two matching-build drafts. Make the next `Storage.prototype.setItem` throw during consumption of the first entry. Assert no text is returned and the original two-entry handoff still exactly matches.

- [ ] **Step 6: Verify rewrite RED**

Run: `npm --prefix web test -- src/drafts/draftStore.test.ts`

Expected: `loadDraft` returns the first text despite the failed rewrite.

- [ ] **Step 7: Verify rewrite GREEN**

The tri-state implementation from Step 3 must make the new test pass without further production behavior. Run the focused draft-store suite and then the four-file recovery suite from Task 1.

### Task 3: Regenerate, verify, commit, and re-review

**Files:**
- Regenerate: `web/bundle/**`

**Interfaces:**
- Produces: deterministic committed PWA bytes whose fingerprint includes the handoff changes.

- [ ] **Step 1: Run source gates**

Run `npm --prefix web ci --no-audit --no-fund`, `npm --prefix web test`, `npm --prefix web run typecheck`, and `npm --prefix web audit --omit=dev`.

- [ ] **Step 2: Build twice and compare every bundle path/hash**

Run `npm --prefix web run build`, capture each `web/bundle/**` SHA-256, repeat the build, and require identical path/hash lists. Record `source-fingerprint.txt`.

- [ ] **Step 3: Run repository gates**

Run `cargo test --lib remote::web:: -j 1`, `rustfmt --edition 2021 --check build.rs src/remote/web/assets.rs src/remote/web/bridge.rs src/remote/web/mod.rs`, `git diff --check`, and the staged `src/pwa/bundle.test.ts` suite.

- [ ] **Step 4: Commit and independently re-review**

Stage only intended source, tests, docs, and generated bundle files. Commit with `fix(web): bind draft handoffs to compatible builds`. Dispatch a fresh reviewer to inspect the committed diff for requirements, lifecycle, and security correctness. Address any substantive findings through another strict RED/GREEN cycle before reporting completion.
