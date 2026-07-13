# PWA Safety and Determinism Remediation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make DevManager PWA activation safe across every live browser tab, make the web-build handshake authoritative before browser actions, and close deterministic-build and cache-policy gaps found in the Task 7 review.

**Architecture:** The waiting service worker is the single authority for global activation: it queries every live window client with a nonce and activates only after safe acknowledgements, then rechecks the client set. Each page separately gates its own reload so a hidden or frozen page cannot lose local work after another tab activates the worker. The WebSocket client treats the host `hello` as a mandatory first protocol frame and emits `open` or sends resume/mutation traffic only after build and protocol validation.

**Tech Stack:** React 18, Zustand, TypeScript, Vitest, Vite PWA/Workbox, Rust, Axum, rust-embed, GitHub Actions.

## Global Constraints

- Work only in `.worktrees/native-mobile-web-task7`; never modify the primary worktree.
- Merge the pinned integration commit `a6ea000` before behavioral implementation.
- Every behavior change follows red-green-refactor; observe the focused test fail for the intended reason before production edits.
- A nonresponding visible client fails activation closed.
- Dead clients cannot leave permanent activation blockers.
- Browser actions, resume, lease acquisition, and composer mutations cannot occur before a valid `hello`.
- `/api/**` and `/pair` responses use `Cache-Control: no-store` on every status path.
- `build.rs` validates only and never installs dependencies, invokes Node, or builds the frontend.

---

### Task 1: Integrate the native mobile UI and Task 7 PWA work

**Files:**
- Merge: `a6ea000`
- Resolve: `src/remote/web/bridge.rs`
- Resolve: `src/remote/web/wire.rs`
- Resolve: `web/src/api/types.ts`
- Resolve: `web/src/store/index.ts`
- Resolve: `web/src/store/index.test.ts`
- Resolve: `web/package.json`
- Resolve: `web/package-lock.json`
- Resolve: `web/src/index.css`
- Resolve: `web/src/vite-env.d.ts`

**Interfaces:**
- Consumes: Task 5/6 `drafts`, `pendingMutations`, attachment-loading state, native routes, semantic resume, and writer lease logic.
- Produces: one integrated tree retaining Task 7 build-ID hello, PWA registration, manifest, worker, and deterministic bundle validation.

- [ ] Merge `a6ea000` with `git merge --no-ff a6ea000` and resolve conflicts by preserving both sides' protocol variants and store behavior.
- [ ] Run `npm --prefix web test` and record any integration failures before remediation.
- [ ] Run `cargo test remote::web --lib` only after the TypeScript merge is coherent.

### Task 2: Global service-worker activation gate

**Files:**
- Modify: `web/src/sw.ts`
- Modify: `web/src/pwa/register.ts`
- Modify: `web/src/pwa/register.test.ts`
- Create: `web/src/pwa/updateProtocol.ts`
- Test: `web/src/pwa/register.test.ts`

**Interfaces:**
- Consumes: `UpdateSafetyState` from the live page and `WindowClient` visibility.
- Produces: nonce messages `DEVMANAGER_UPDATE_SAFETY_QUERY`, `DEVMANAGER_UPDATE_SAFETY_ACK`, and `DEVMANAGER_ACTIVATE_UPDATE`; an activation result that never skips waiting on an unsafe cohort.

- [ ] Add failing tests for two safe clients, one unsafe client, visible timeout, hidden/frozen timeout plus local reload deferral, client disappearance, and a client appearing between the first query and final re-enumeration.
- [ ] Run the focused Vitest file and confirm each test fails because global coordination is absent.
- [ ] Implement a bounded query coordinator with injected client enumeration, nonce generation, and timeout scheduling so it is unit-testable without a browser.
- [ ] Route activation messages through the waiting worker; re-enumerate and query newly appeared clients before `skipWaiting()`.
- [ ] Pass a custom controlling/reload callback to Vite registration; reload only when the current page is locally safe, retrying on store and visibility changes.
- [ ] Run focused tests green, then the full PWA test group.

### Task 3: Authoritative web hello handshake

**Files:**
- Modify: `web/src/api/ws.ts`
- Modify: `web/src/api/ws.test.ts`
- Modify: `web/src/store/index.ts`
- Modify: `web/src/pwa/buildCompatibility.ts`
- Modify: `web/src/pwa/buildCompatibility.test.ts`

**Interfaces:**
- Consumes: host `hello { protocolVersion, webBuildId }`.
- Produces: a validated-ready callback/status; incompatibility callback carrying host build ID; no resume, heartbeat, lease, or mutation traffic before readiness.

- [ ] Add failing tests for missing first hello, wrong frame type, wrong protocol, wrong build, valid hello, and pending composer mutation held until validation.
- [ ] Run the focused WebSocket tests and confirm protocol traffic currently occurs before validation.
- [ ] Move compatibility validation into `WsClient`; distinguish transport-open from protocol-ready.
- [ ] Emit `open`, start heartbeat, resume, and composer retry only after a valid hello.
- [ ] Make malformed/missing/incompatible hello fail closed and invoke the existing compatible-build recovery path exactly once.
- [ ] Run focused and full web tests green.

### Task 4: Actual-store PWA safety state

**Files:**
- Modify: `web/src/pwa/storeSafety.ts`
- Modify: `web/src/pwa/storeSafety.test.ts`
- Modify: `web/src/store/index.ts`
- Modify: `web/src/store/index.test.ts`
- Modify: `web/src/sessions/SessionScreen.tsx`
- Modify: `web/src/sessions/Composer.tsx`

**Interfaces:**
- Consumes: live Zustand `drafts`, `pendingMutations`, selected attachments, and attachment-loading count/state.
- Produces: `UpdateSafetyState` whose unsafe flag covers any raw non-empty text, unsent attachment, attachment read, or pending composer mutation.

- [ ] Add an integration test importing the real Zustand store and proving raw whitespace, attachments, loading, and mutations block activation while a fully empty composer permits it.
- [ ] Run the integration test red against the merged store.
- [ ] Add the minimal store fields/selectors needed to expose attachment and attachment-read safety without duplicating composer truth.
- [ ] Remove `.trim()` semantics from draft safety and subscribe the PWA coordinator to every safety field.
- [ ] Run focused store/composer/PWA tests green.

### Task 5: HTTP no-store and security headers

**Files:**
- Modify: `src/remote/web/mod.rs`
- Modify: `src/remote/web/assets.rs`
- Test: `src/remote/web/mod.rs`
- Test: `src/remote/web/assets.rs`

**Interfaces:**
- Consumes: every `/api/**` and `/pair` Axum response, including redirect and failure paths.
- Produces: `Cache-Control: no-store` on protected dynamic responses and existing static cache/security policy on embedded assets.

- [ ] Add failing route tests for health, authenticated/unauthenticated `me`, successful pairing redirect, invalid pairing, throttled pairing, and unknown API/pair routes.
- [ ] Run the focused Rust tests and confirm missing `no-store` headers.
- [ ] Add a scoped Axum middleware or response helper applying `no-store` to dynamic protected routes without changing immutable static caching.
- [ ] Tighten CSP WebSocket sources only if same-host proxy tests remain functional; otherwise retain the documented functional policy.
- [ ] Run focused Rust tests green.

### Task 6: Deterministic tracked bytes and pre-merge CI

**Files:**
- Create: `.gitattributes`
- Modify: `.github/workflows/release.yml`
- Create or modify: `.github/workflows/ci.yml`
- Modify tests: `web/src/pwa/bundle.test.ts`

**Interfaces:**
- Consumes: generated `web/bundle/**` artifacts.
- Produces: byte-preserving Git checkout rules and a pull-request clean-bundle probe using the same supported frontend build path as release.

- [ ] Add a failing checkout-filter assertion showing generated bundle blobs change under `core.autocrlf=true` without attributes.
- [ ] Add `.gitattributes` with `web/bundle/** -text` and verify filtered hashes equal repository blobs.
- [ ] Add a pull-request workflow that runs `npm --prefix web ci`, `npm --prefix web run build`, fails on bundle drift, and runs focused Rust asset tests.
- [ ] Keep release on the same commands and ensure neither workflow delegates frontend generation to `build.rs`.

### Task 7: Regenerate, verify, review, and commit

**Files:**
- Regenerate: `web/bundle/**`
- Review: all changes from `f27ebac..HEAD`

**Interfaces:**
- Consumes: all remediated sources.
- Produces: one internally consistent tracked bundle and a reviewable remediation commit.

- [ ] Run `npm --prefix web ci --no-audit --no-fund`.
- [ ] Run `npm --prefix web test`.
- [ ] Run `npm --prefix web run typecheck`.
- [ ] Run `npm --prefix web run build` twice and verify the second run leaves `web/bundle` unchanged.
- [ ] Run `npm --prefix web audit --omit=dev` and record the full audit separately.
- [ ] Run `cargo test remote::web::assets --lib` and all new focused web-route/handshake tests.
- [ ] Run `git diff --check` and inspect `git status --short`.
- [ ] Search `build.rs` for process runners and confirm none exist.
- [ ] Request code review, address verified findings with TDD, and rerun the full verification set.
- [ ] Commit the integrated remediation and report the commit SHA.
