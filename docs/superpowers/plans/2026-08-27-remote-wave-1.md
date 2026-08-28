# Remote Wave 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Remove the browser key-custody dead end and fix foreground route continuity without advertising unfinished remote execution.

**Architecture:** Keep the shared Rust/WASM Noise core. Persist wrapped device key material with a non-extractable AES-GCM key and construct the existing transport privately. Align legacy projection resume with the already-shipping task routes while later waves replace its command adapter.

**Tech Stack:** Existing Rust/Snow/WASM, TypeScript/WebCrypto/IndexedDB, React/Vitest.

**Spec:** `docs/superpowers/specs/2026-08-27-seamless-remote-work-design.md`

## Execution evidence (2026-08-27)

The custody, lifecycle and route implementation is integrated. This is a
foundation, not working multi-device remote execution.

- Browser suite at the wave-1 boundary: 425 tests passed across 63 files; web
  production build/typecheck passed. Build-script regression: 1 passed;
  PowerShell Connect build contract: 12 passed.
- A real browser using IndexedDB, non-extractable wrapping keys and generated
  WASM retained the same identity across a page reload and a host-generation
  decrease. The probe used an isolated loopback origin, not production pairing.
- First isolated Rust check passed. Serial library run: 3158 passed, 2 failed,
  1 ignored. One stale notification route expectation is corrected. The Git
  repository-validation test passed five isolated reruns; its intermittent
  full-suite failure remains recorded, not relabeled as a clean suite.
- Installed PID/start time and production config.json/remote.json hashes stayed
  unchanged; the exact verification processes exited.
- Wave 2 exposed an additional real-wire gap: generic JSON payload decoding
  rejected native MessagePack binary UUIDs. The decoder now preserves binary
  fields, and native/generated-WASM fixtures are being added. JSON-only crypto
  round trips are insufficient proof of host conversation compatibility.

Final union gates and the local commit remain pending. The task checklist below
is the implementation contract; this evidence section records the current gate
state and does not imply LAN, WAN, phone or cross-PC acceptance.

## Global constraints

- No plaintext fallback, new crypto primitives, silent identity rotation or public listener enablement.
- Preserve root AGENTS.md and the user's running development/installed apps.
- Reuse current transport, lifecycle, route and draft code; no parallel stores.
- Write regression cases with the implementation; user requests consolidated execution at the end.
- Cursor implementation workers own only their listed paths; no commits or recursive delegation.

## Task 1: Browser custody and transport construction

**Files:** `web/src/connect/{identity,crypto,transport}.ts`, their tests; only if needed `crates/connect-crypto/src/wasm.rs` and crypto export tests.

**Consumes:** Existing `ConnectBrowserTransportOptions`, generated `WasmConnectHandshake`, `ConnectIdentityStorage`, and authenticated host publication.

**Produces:** A versioned wrapped identity record and bounded private handshake factory; `bootstrapConnect` no longer has an unconditional key-custody HOLD when its prerequisites are genuinely available.

- [ ] Add real WebCrypto cases for create/load, persistence across host generation,
  rejected ciphertext/metadata tampering, invalid wrapping key, unavailable storage,
  and old v1 opaque-key records requiring explicit repair rather than replacement.
- [ ] Generate a standard X25519 keypair via the existing reviewed implementation
  or WebCrypto import/export boundary; use a non-extractable AES-GCM key for storage.
  Bind record version/device/public identity as AAD. Validate lengths/algorithms,
  authenticate before use, and zero temporary private byte buffers on every path.
- [ ] Replace long-lived raw-key transport options with a private handshake factory
  where required; preserve byte-option compatibility only for existing bounded
  fixtures. Production transport must not retain an unwrapped key between connects.
- [ ] Construct transport only after pairing check, valid publication, compatible
  WASM and valid stored custody. Publish Connect selection before asynchronous work
  so any error remains fail-closed, never an accidental legacy socket.
- [ ] Keep projection callbacks a required integration boundary: do not claim
  canonical task execution from an encryption-only handle. Missing capability,
  artifact or adapter remains explicit. Preserve existing public host pins.
- [ ] Add bootstrap tests through real custody plus a bounded fake WASM boundary;
  verify no private bytes enter runtime publication, storage plaintext or errors.

Final checks (once after coherent edits):

```powershell
npm --prefix web test -- src/connect/identity.test.ts src/connect/crypto.test.ts src/connect/transport.test.ts src/connect/storeAdapter.test.ts
npm --prefix web run typecheck
```

## Task 2: Foreground route continuity

**Files:** `src/remote/web/bridge.rs`, `web/src/app/router.ts`, `web/src/tasks/taskId.ts`, `web/src/platform/lifecycle.ts`, existing adjacent tests only as needed.

**Consumes:** Current route generator and `ResumeContext` stable session key.

**Produces:** Host resume accepts exactly the task routes generated by the PWA;
forged/mismatched routes still clear selection instead of attaching another task.

- [ ] Add literal `/tasks/<encoded tab:...>` and server-resource route cases,
  malformed escaping, foreign task key and old supported route cases to the real
  `validate_resume_route`/resume consumer tests.
- [ ] Reuse canonical task ID decoding rules; accept valid modern task routes
  without weakening runtime revision and semantic cursor validation.
- [ ] Test coalesced foreground notifications and stale-generation responses
  through existing client lifecycle/store consumers. Fix only demonstrated gaps;
  preserve drafts, selected task and mounted cached content during reconciliation.
- [ ] Keep semantic replay validation and provider-session correlation unchanged.

Final checks are included in the root isolated Rust check/library test gate and
the existing web router/lifecycle/store suites. No duplicate broad builds.

## Root review and handoff

- [ ] Inspect complete source/status diff; ensure no unrelated files changed.
- [ ] Join exact worker wrapper and descendants before integrating.
- [ ] Run the focused web gate, then one consolidated Rust gate if Rust changed.
- [ ] Record wave 1 as foundation only. Duplex push, canonical task actions,
  trusted LAN provisioning, multi-host native views and WAN remain later waves;
  do not present this wave as working multi-device product acceptance.
