# Push Intent Reconciliation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make persisted per-browser notification intent the single host truth and make endpoint repair atomic, exact, and race-safe across foreground resumes, explicit toggles, service-worker rotation, delivery expiry, key rotation, and client revocation.

**Architecture:** `WebPushConfig` persists enabled client IDs separately from endpoint records; normalization migrates legacy subscriptions into enabled intent. Registration mutations carry an explicit `enable` or `reconcile` mode, execute under the existing atomic host-config mutation lock, and replace every endpoint for that client only when intent permits it. The browser serializes all push state transitions, treats the host response as authoritative, and removes local subscriptions whenever the host is disabled.

**Tech Stack:** Rust, Axum, Serde, React/TypeScript, Service Workers, Vitest, Vite PWA.

## Global Constraints

- Follow strict red-green-refactor TDD for every production behavior.
- Preserve signed-cookie authentication, same-origin `Origin` validation, JSON-only mutations, endpoint allowlisting, and generic push content.
- Preserve the global subscription bound and make the exact-replacement invariant stricter than the existing per-client bound.
- Existing persisted subscriptions imply enabled intent on load.
- A 404/410 delivery or VAPID key rotation removes endpoints without removing enabled intent.
- Revocation and reset remove both endpoints and intent.
- Rebuild the tracked deterministic bundle twice and require a clean second build.

---

### Task 1: Persisted notification intent and exact endpoint replacement

**Files:**
- Modify: `src/remote/web/push.rs`
- Modify: `src/remote/web/mod.rs`
- Modify: `src/remote/mod.rs`

**Interfaces:**
- Produces: `WebPushConfig::notifications_enabled(&str) -> bool`.
- Produces: `WebPushConfig::enable_and_replace_subscription(...)` and `reconcile_and_replace_subscription(...) -> bool`.
- Produces: `WebPushConfig::disable_client(&str)` and revocation-safe `remove_client(&str)`.

- [ ] **Step 1: Write failing persistence and lifecycle tests**

Add tests that deserialize legacy JSON without the new intent field, normalize it, and assert each subscription client becomes enabled; assert `ensure_keys()` and dispatcher 410 removal keep intent; assert revocation removes intent.

- [ ] **Step 2: Run tests to verify RED**

Run: `cargo test remote::web::push::tests::legacy_subscriptions_migrate_to_enabled_intent --lib -- --exact`

Expected: FAIL because enabled intent and its query API do not exist.

- [ ] **Step 3: Implement minimal persisted intent model**

Persist a bounded, deduplicated collection of enabled client IDs. Normalize legacy subscriptions into that collection, leave the collection untouched when keys or expired endpoints are removed, and remove it with client revocation/reset.

- [ ] **Step 4: Add exact-replacement tests and implementation**

Test that enable replaces all stale endpoints for one client, reconcile replaces only while enabled, and disable clears intent plus all client endpoints. Implement those three mutations while retaining global eviction for other clients.

- [ ] **Step 5: Run focused Rust tests GREEN**

Run: `cargo test remote::web::push --lib`

Expected: all push tests pass.

### Task 2: Atomic authenticated host API

**Files:**
- Modify: `src/remote/web/push.rs`
- Modify: `src/remote/web/mod.rs`

**Interfaces:**
- Consumes: Task 1 intent and replacement methods.
- Produces: registration request mode `enable | reconcile`.
- Produces: JSON mutation response `{ enabled: boolean }` and status response whose enabled value is intent, not endpoint presence.

- [ ] **Step 1: Write failing route tests**

Add route tests proving explicit enable atomically sets intent and leaves exactly the submitted endpoint, reconcile while disabled cannot add an endpoint, reconcile after enable replaces stale endpoints, explicit disable sets false and removes all endpoints, and a delayed reconcile cannot resurrect after disable.

- [ ] **Step 2: Run route tests RED**

Run: `cargo test remote::web::tests::push_ --lib`

Expected: new intent/atomicity assertions fail against endpoint-presence status and endpoint-scoped unsubscribe.

- [ ] **Step 3: Implement atomic handlers**

Parse explicit registration mode, validate the subscription before mutation, and perform intent check plus exact replacement in one `mutate_host_config` closure. Make explicit disable one mutation that clears intent and every client endpoint. Preserve legacy request compatibility without allowing a legacy automatic POST to re-enable disabled intent.

- [ ] **Step 4: Verify route security and behavior GREEN**

Run: `cargo test remote::web::tests --lib`

Expected: all route, CSRF, persistence, and no-store tests pass.

### Task 3: Serialized browser reconciliation and service-worker rotation

**Files:**
- Modify: `web/src/pwa/notifications.ts`
- Modify: `web/src/pwa/notifications.test.ts`
- Modify if needed: `web/src/App.tsx`
- Modify: `web/src/sw.ts`

**Interfaces:**
- Consumes: Task 2 `{ enabled }` responses and registration modes.
- Produces: one serialized/coalesced automatic reconciliation lane shared with explicit enable/disable.

- [ ] **Step 1: Write failing host-intent browser tests**

Test host-disabled/browser-present cleanup with no registration POST; host-enabled/browser-missing recreation; host-enabled/wrong-key replacement; exact endpoint/key POST in reconcile mode; and host-disabled service-worker rotation without subscription creation.

- [ ] **Step 2: Run Vitest RED**

Run: `npm test -- --run src/pwa/notifications.test.ts`

Expected: host-disabled and operation-mode assertions fail.

- [ ] **Step 3: Implement host-authoritative reconciliation**

When disabled, unsubscribe any local subscription and return disabled. When enabled, heal missing or wrong-key local state, then issue atomic reconcile and honor its returned enabled value; if it lost a race with disable, remove the local subscription.

- [ ] **Step 4: Write and verify disable-race RED**

Use deferred fetch promises to start foreground reconcile, execute explicit disable, then release the reconcile response. Assert serialized execution prevents any post-disable registration and leaves local plus host state disabled.

- [ ] **Step 5: Implement shared operation serialization GREEN**

Queue explicit toggles and coalesce automatic foreground operations through one module-level operation lane. Ensure callers receive the authoritative final state and automatic lifecycle errors remain retryable on a later foreground.

- [ ] **Step 6: Verify service-worker rotation GREEN**

Test that `pushsubscriptionchange` calls reconcile mode, follows host intent, registers the replacement atomically, and never performs a stale endpoint cleanup request.

Run: `npm test -- --run src/pwa/notifications.test.ts`

Expected: all notification tests pass.

### Task 4: Full verification and deterministic bundle

**Files:**
- Regenerate: `web/bundle/**`

- [ ] **Step 1: Run format, type, and focused suites**

Run: `cargo fmt --check`, `npm --prefix web run typecheck`, focused Rust push/web tests, and focused notification/bundle tests.

- [ ] **Step 2: Run full suites**

Run: `cargo test --lib` and `npm --prefix web test`.

- [ ] **Step 3: Rebuild deterministic bundle twice**

Run `npm --prefix web run build`, record `git status --short -- web/bundle`, run the same build again, and require `git status --short -- web/bundle` to be identical after the second run.

- [ ] **Step 4: Request independent code review**

Review the complete diff specifically for intent migration, atomicity, lock ordering, old-bundle compatibility, iOS/WebKit behavior, auth/CSRF/privacy, and bundle determinism; fix every critical or important finding under TDD.

- [ ] **Step 5: Commit intended files**

Stage only the plan, Rust source/tests, TypeScript source/tests, and generated bundle. Commit with `fix(remote): make push intent authoritative` and report the SHA plus exact verification counts.
