# Phase 9: Connect Realtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let desktop, phone, browser, and invited viewers participate in the same local Task in realtime—directly on the LAN or through optional DevManager Connect—while the local Rust host remains execution authority, paired devices survive upgrades, and the hosted relay cannot read raw task content.

**Architecture:** The existing open-source React PWA becomes a projection client for the Phase 1 protocol, with responsive full-screen Task modes on phones. The host exposes a direct authenticated WebSocket path and maintains one outbound connection to the hosted Connect relay for NAT traversal. Routing/presence metadata is visible to Connect; chunked snapshots, durable events, ephemeral terminal/browser frames, prompt-library queries, and commands use a separately authenticated end-to-end channel between persisted device identities and the host. Every mutation is idempotent and kernel-serialized; solo users switch devices invisibly, while separate expiring task invitations enable view or collaborate/write access.

**Tech Stack:** Rust/Tokio/Axum/tokio-tungstenite, versioned MessagePack, a shared Rust/WASM `connect-crypto` leaf using fixed `Noise_XX_25519_ChaChaPoly_BLAKE2s` first-pairing and `Noise_IK_25519_ChaChaPoly_BLAKE2s` pinned-device channels, React 18/TypeScript/Vite PWA, Express/Sequelize/PostgreSQL/Socket.IO for proprietary routing and presence.

## Global Constraints

- The local host is the only execution authority. Neither PWA nor Connect API may launch providers, edit files, answer approvals, or mutate task truth except through a host command receipt.
- Paired device identity, device credential, host identity, pairing secret, authorized-device records, and manually changeable long-lived device pairing code persist in `remote.json`/OS credential storage and do not rotate on application or web-bundle updates.
- Pairing-code rotation and per-device revocation are explicit and independent. Updating must not revoke a paired device.
- Task invitations are separate scoped credentials with nickname, expiry, individual revocation, and independent view/collaborate-write capabilities. They never reuse/reveal the pairing code or grant owner authority.
- Direct and hosted clients use the same semantic commands/events and request IDs. Transport-specific DTOs do not become kernel persistence models.
- Realtime means ordered push with local optimistic echo and authoritative receipt; no page refresh/poll loop is required to observe current task, approval, terminal, browser, service, or connection state.
- Realtime uses bounded chunked initial state, durable ordered replay, and coalescible ephemeral streaming/status lanes. Large child transcripts, terminal history, diffs, recordings, prompt bodies, and artifacts are fetched incrementally on demand.
- Raw prompts/responses, terminal, browser, recordings, file bodies, and full diffs are encrypted end to end and shared only with explicitly authorized clients. Relay storage/logs never contain their plaintext.
- Do not design new cryptographic primitives. Select a specified protocol/pattern and maintained implementations, pin versions, use cross-language official/golden vectors, run dependency/advisory review, and obtain an independent security review before public hosted rollout.
- Pairing attempts are rate limited and visible; a human-readable code alone cannot silently authorize unbounded offline guessing. QR/link pairing pins the host identity, and manual pairing requires a verified short-authentication/fingerprint step if the code lacks cryptographic entropy.
- Treat remote prompt/input as remote code-execution authority because the stock provider can invoke shell, filesystem, Git, browser, and service tools; every grant and UI warning reflects that authority.
- Pairing secrets never appear in URL query strings, browser history, referrers, analytics, relay logs, or push payloads. Direct web endpoints enforce TLS, exact Origin policy, CSRF-resistant state changes, CSP, safe cache headers, and bounded request bodies.
- Presence/control remains invisible for the single owner. Collaboration controls appear only after an invite exists.
- Dangerous approvals are Owner-only by default. Watcher is always read-only. Collaborator rights are Task-scoped, bounded, expiring/revocable, and checked again locally.
- All relay/reconnect/encryption/bundle probes run outside terminal/UI/browser hot paths with bounded queues and priority lanes.
- Paired owner devices may access the Phase 7 personal prompt library through the E2E host channel. Connect stores no personal prompt copy and this phase adds no cross-host synchronization or prompt-specific cryptography.

---

## Repository and file map

**Open-source DevManager (`C:\Code\userfirst\devmanager`):**

- Create: `src/connect/mod.rs`
- Create: `src/connect/model.rs`
- Create: `src/connect/direct.rs`
- Create: `src/connect/relay.rs`
- Create: `src/connect/identity.rs`
- Create: `src/connect/invites.rs`
- Create: `src/connect/crypto.rs`
- Create: `src/connect/session.rs`
- Create: `src/connect/projection.rs`
- Create: `src/connect/permissions.rs`
- Create: `src/connect/presence.rs`
- Create: `src/connect/push.rs`
- Refactor: `src/remote/{mod,transport,client_pool,presentation,access_log}.rs`
- Refactor/delete at cutover: `src/remote/web/{wire,auth,bridge,lease,request_executor,input_executor,push,dto}.rs`
- Modify: `src/config/remote_store.rs`
- Modify: `src/protocol/{crypto,capabilities,envelope,frame}.rs`
- Modify: `src/domain/{command,event,snapshot}.rs`
- Modify: `src/kernel/{outbox,command_bus}.rs`
- Modify: `src/host/mod.rs`
- Modify: `src/client/action.rs`
- Modify: `web/src/{App,main}.tsx`
- Create: `web/src/protocol/*`
- Create: `web/src/connect/*`
- Create: `web/src/tasks/*`
- Create: `web/src/prompts/*`
- Create: `web/src/browser/*` (begun in Phase 8)
- Refactor: `web/src/sessions/*`
- Modify: `web/src/store/index.ts`
- Modify: `web/src/pwa/*`

**Proprietary Connect service (`C:\Code\happier\portal\api` and `web`):**

- Create: `api/src/database/models/devmanager/ConnectHost.ts`
- Create: `api/src/database/models/devmanager/ConnectDevice.ts`
- Create: `api/src/database/models/devmanager/ConnectRouteTicket.ts`
- Create: `api/src/database/models/devmanager/ConnectPresence.ts`
- Create: `api/src/database/migrations/20260804000000-create-devmanager-connect-routing.cjs`
- Create: `api/src/routes/devmanagerConnectRoutes.ts`
- Create: `api/src/controllers/devmanagerConnectController.ts`
- Create: `api/src/services/devmanagerConnect/{tickets,relay,presence,rateLimit}.ts`
- Modify: `api/src/routes/index.ts`
- Create: `web/src/api/devmanagerConnect.ts`
- Create: `web/src/types/devmanagerConnect.ts`
- Create: `web/src/pages/devmanager/ConnectPage.tsx`
- Create: `web/src/components/devmanager/ConnectFrame.tsx`
- Modify: `web/src/App.tsx`
- Modify: `web/src/pageRegistry.ts`
- Modify: `api/src/database/index.ts`

**Tests/scripts:**

- Create: `tests/connect_identity.rs`
- Create: `tests/connect_crypto.rs`
- Create: `tests/connect_session.rs`
- Create: `tests/connect_permissions.rs`
- Create: `tests/connect_realtime.rs`
- Create: `web/src/protocol/*.test.ts`
- Create: `web/src/connect/*.test.tsx`
- Create: `web/src/tasks/*.test.tsx`
- Create: `web/src/prompts/*.test.tsx`
- Create: `api/src/services/devmanagerConnect/{tickets,relay,presence,rateLimit}.test.ts`
- Create: `api/src/routes/devmanagerConnectRoutes.test.ts`
- Create: `web/src/components/devmanager/ConnectFrame.test.tsx`
- Create: `tests/fixtures/connect/v1/*`
- Create: `scripts/native-next/Invoke-ConnectE2E.ps1`
- Create: `docs/security/connect-threat-model.md`
- Create: `docs/adr/0002-connect-e2e-transport.md`
- Create after the dual-target proof: `crates/connect-crypto/Cargo.toml`
- Create after the dual-target proof: `crates/connect-crypto/src/lib.rs`
- Create: `crates/connect-crypto/tests/vectors.rs`
- Create: `scripts/native-next/Build-ConnectCrypto.ps1`

Before editing either Portal repo, read and follow its local `AGENTS.md`, create an isolated worktree/branch for that repo, record baseline gates, and keep commits repository-local.

Portal paths above are relative to `C:\Code\happier\portal`; commands use `npm --prefix C:\Code\happier\portal\api` and `npm --prefix C:\Code\happier\portal\web` so each separate Git repository uses its own lockfile and agent guidance.

### Task 9.1: Freeze one cross-language Connect wire contract

**Files:** `src/connect/model.rs`, `src/protocol/{capabilities,envelope,chunk}.rs`, `web/src/protocol/{types,codec,schema}.ts`, `tests/fixtures/connect/v1/*`, `tests/connect_session.rs`, `web/src/protocol/*.test.ts`

- [ ] **Step 1: Write paired Rust/TypeScript failing tests** that decode the same golden hello/negotiated limits, capability-filtered ActionCatalog descriptors, snapshot page, event page, command, accepted receipt, operation settlement, presence, terminal delta, browser frame metadata, prompt metadata/version/search/chain pages, chunk/resume, generic unknown extension, resync, and error fixtures.
- [ ] **Step 2: Run** `cargo test --test connect_session wire_ -- --nocapture` and `npm --prefix web test -- protocol` and record both red results.
- [ ] **Step 3: Define an explicit Connect envelope** with protocol major/minor, connection/session IDs, monotonically increasing channel sequence, request/operation IDs, negotiated Phase 1 physical/reassembled/page/chunk limits, reserved compression field fixed to `None` in v1, privacy class, payload discriminant/version, and bounded payload. Browser images remain independently encoded, while the E2E envelope itself is never compressed.
- [ ] **Step 4: Use MessagePack for binary transport** and one committed schema/fixture generator invoked by `cargo test`/npm check mode. Generate TypeScript discriminated unions/codecs from the reviewed Rust/schema source rather than maintaining hand-divergent field names.
- [ ] **Step 5: Separate three semantic layers with priority subchannels:** critical mutations/receipts/queries/replies/settlements/interactive requests; durable ordered state/event pages; ephemeral terminal/browser/progress/status frames. Define per-channel limits, coalescing, disconnect, replay, and resync behavior in fixtures. Child transcripts, terminal scrollback, diffs, recordings, prompt bodies, and artifacts are on-demand paged/chunked transfers, never part of the initial Task snapshot.
- [ ] **Step 6: Make Rust schema/golden fixtures authoritative.** Generate TypeScript discriminated unions/codecs or verify every decoder against those fixtures; unknown future optional payloads render bounded generic data and cannot become an interactive action or domain transition.
- [ ] **Step 7: Run** both language suites and prove byte-for-byte fixture agreement; commit DevManager changes as `feat(connect): freeze cross language wire contract`.

### Task 9.2: Preserve host/device identity and pairing across updates

**Files:** `src/connect/identity.rs`, `src/config/remote_store.rs`, `src/remote/mod.rs`, `tests/connect_identity.rs`, `web/src/connect/{identity,pairing}.ts`, `web/src/connect/*.test.tsx`

- [ ] **Step 1: Write failing tests** using current `remote.json` fixtures for long-lived pairing-code stability, native/web device records, upgrade round-trip, device-key persistence, explicit pairing-code rotation, single-device revocation, all-device revocation, copied profile rejection, and corrupt credential recovery.
- [ ] **Step 2: Run** `cargo test --test connect_identity -- --nocapture` and save the red output.
- [ ] **Step 3: Preserve current pairing/code fields** without load-time or update-time regeneration. Add stable host/device public identity and key references through additive versioned fields; private keys live in OS credential storage, not plaintext logs/events. Do not use these fields for Task invitations.
- [ ] **Step 4: On first cryptographic identity setup**, create it explicitly during Connect enable/pairing and persist atomically. Do not silently replace a valid identity because a binary version changed.
- [ ] **Step 5: Store browser device private identity** in WebCrypto non-exportable IndexedDB where supported; define a visible re-pair flow for cleared browser storage, not an automatic new identity.
- [ ] **Step 6: Make rotation semantics explicit:** changing the pairing code affects future pairing only unless the user separately revokes devices; rotating host identity warns that all devices require re-pairing. Task-invitation rotation/revocation is a separate command and store.
- [ ] **Step 7: Run** identity tests including simulated upgrades over several build numbers; commit as `fix(connect): preserve pairing identity across updates`.

### Task 9.3: Replace the direct web bridge with a host command/event client

**Files:** `src/connect/{direct,session,projection,permissions}.rs`, `src/remote/web/{wire,auth,bridge,request_executor,input_executor}.rs`, `src/host/mod.rs`, `tests/connect_realtime.rs`

- [ ] **Step 1: Write failing tests** where a direct client pairs, resumes a chunked snapshot, receives durable/ephemeral events, creates/selects a task, queries the personal prompt library, sends provider input, answers an approval, receives terminal/browser updates, fetches one child transcript on demand, reconnects from cursor, and resyncs after overflow; also test wrong Origin, plaintext downgrade, CSRF attempt, pairing token in URL/referrer, oversized body, CSP/cache headers, and pairing rate limits.
- [ ] **Step 2: Run** `cargo test --test connect_realtime direct_ -- --nocapture` and record the red result.
- [ ] **Step 3: Terminate direct TLS/WebSocket transport in a host-owned service** with exact Origin allowlisting, no credential in URL, one-time POST pairing exchange, strict CSP/security/cache headers, bounded bodies/frames, and per-source/pairing-code rate limits; translate authenticated frames only into `HostClient`-equivalent mutations/queries/subscriptions. Loopback may use the browser-defined trustworthy `localhost` origin. LAN binding is opt-in and requires a configured certificate whose SAN matches the advertised host name and whose chain the device trusts; there is no plaintext LAN or click-through certificate fallback. The hosted relay remains the zero-certificate mobile path. Remove presentation-specific mutations from `bridge.rs` incrementally and record each old path in the deletion ledger.
- [ ] **Step 4: Authorize every mutation, query, and subscription locally** from paired device/Task grant/privacy class/current request state; network authentication does not imply personal-prompt, filesystem, process, or approval authority.
- [ ] **Step 5: Use command/operation IDs for reconnect retry/status** and cursor-based replay. On missing cursor/overflow, send `ResyncRequired` and fresh bounded snapshot pages without browser refresh. Accepted operations remain pending until settlement/status proves their outcome.
- [ ] **Step 6: Prove a slow direct client** cannot delay host command receipts or terminal/browser readers; commit as `refactor(connect): route direct clients through kernel protocol`.

### Task 9.4: Rebuild the PWA around Tasks and phone-native modes

**Files:** `web/src/{App,main}.tsx`, `web/src/connect/*`, `web/src/tasks/*`, `web/src/prompts/*`, `web/src/sessions/*`, `web/src/store/index.ts`, `web/src/index.css`, `web/src/pwa/*`, associated tests

- [ ] **Step 1: Write failing React tests** for Task Inbox, Chat default, full-screen Changes/Files/Terminal/Browser/Services/Artifacts modes, owner-only personal Prompt Library search/create/edit/version/diff/archive/history/chain insert-between, manual chain **Put in composer**, task switch, local draft, optimistic accepted/pending/settled/failed/cancelled/uncertain command state, reconnect/resync, offline draft, stale bundle, mobile safe-area/keyboard behavior, malicious Markdown/HTML/URL/title/filename/provider event, generic unknown-event fallback, and external-link confirmation.
- [ ] **Step 2: Run** `npm --prefix web test -- tasks connect` and save the red results.
- [ ] **Step 3: Replace session-tab routing** with stable Task routes and models. Keep terminal/server details as resources under a Task; no web component invents provider/terminal ownership.
- [ ] **Step 4: Implement responsive anatomy:** desktop mirrors Inbox/content/dock; phone shows one full-screen mode with bottom/tab navigation, sticky task header/composer, touch-sized controls, safe-area padding, and no miniature pane layout. Render all host/provider/browser/file text as untrusted, disable raw HTML, sanitize/bound links and labels, and require confirmation before opening external schemes.
- [ ] **Step 5: Keep per-device view state and drafts local**, keyed by host/Task/Agent. Build menus/composer/actions from host-supplied shared ActionIds/capability descriptors; optimistic entries reconcile by command/operation ID through pending/settled/failed/cancelled/uncertain states. Rejection remains visible with retry/edit; uncertainty explains that the effect may have occurred and offers inspection/reconciliation, never a one-click implicit resend.
- [ ] **Step 6: Render the Phase 7 prompt projection for paired owner devices** with paged search, create/edit-as-version, version history/diff, archive/restore, recent-history controls, ordered chain editing/insert-between, and exact-version **Put in composer**. Mutations use ordinary host `CommandEnvelope` receipts/settlements; Task invitations do not grant personal prompt-library access. The web client keeps no authoritative/cloud copy, does not cross-host sync, and never auto-sends/advances a chain.
- [ ] **Step 7: Drive all updates from the live connection store.** Service worker caches the shell/assets only, never stale live task/prompt responses; incompatible bundle shows update/reload while preserving device identity/drafts.
- [ ] **Step 8: Validate at iPhone/Android widths, landscape, desktop, throttled CPU/network, offline/reconnect, and install mode**; commit as `feat(connect): make the pwa a realtime task client`.

### Task 9.5: Select and prove the end-to-end encrypted channel

**Files:** `Cargo.toml`, `crates/connect-crypto/{Cargo.toml,src/lib.rs,tests/vectors.rs}`, `src/connect/crypto.rs`, `src/protocol/crypto.rs`, `web/src/connect/{crypto.ts,crypto.test.ts}`, `scripts/native-next/Build-ConnectCrypto.ps1`, `tests/connect_crypto.rs`, `tests/fixtures/connect/v1/crypto-*`, `docs/{security/connect-threat-model,adr/0002-connect-e2e-transport}.md`

**Protocol references:** the official Noise revision/pattern definitions at `https://noiseprotocol.org/noise.html`; implementation candidates and current security advisories must be recorded by exact repository/version in ADR 0002 rather than inferred from package names.

- [ ] **Step 1: Write the threat model first:** malicious/curious relay, passive observer, active frame injection/replay/reorder, stolen routing token, guessed long-lived pairing code, stolen task-invitation credential, revoked device, cloned browser storage, host compromise, metadata leakage, and denial of service. Explicitly state what E2E cannot protect.
- [ ] **Step 2: Prove the shared implementation choice** by compiling one minimal `snow`-backed Rust core for native and `wasm32-unknown-unknown`, measuring release WASM/bundle size and handshake latency, and running its upstream vectors. Record the exact pinned `snow`/wasm-bindgen versions, maintenance, audits/advisories, compile cost, key storage, and the explicit reason this security/cross-target boundary justifies the program's only extra Rust package. If the same core cannot pass both targets, stop for an architecture/security review rather than writing a second Noise implementation.
- [ ] **Step 3: Produce a red interoperability harness** with official protocol vectors plus DevManager transcript/prologue/payload vectors, tamper, replay, wrong device, revoked key, sequence exhaustion, reconnect, and corrupted ciphertext. The same vectors must execute in Rust and the browser.
- [ ] **Step 4: Lock ADR 0002 before relay code:** use `Noise_XX_25519_ChaChaPoly_BLAKE2s` with QR-pinned host static key or verified short-authentication string for first owner pairing or Task-invitation redemption. Inside that encrypted handshake, owner pairing presents only the stable pairing code, while guest redemption presents only the scoped invitation credential; the flows have distinct transcript labels and cannot substitute for one another. A redeemed guest key is bound only to its Task grant. Use `Noise_IK_25519_ChaChaPoly_BLAKE2s` after the device pins the host and the host authorizes the owner device or guest grant; bind `DevManagerConnect/v1`, route/session IDs, protocol major, and credential purpose in the prologue; reject runtime algorithm negotiation/downgrade; reconnect with a new handshake before Noise nonce exhaustion or one-hour session age.
- [ ] **Step 5: Implement the one shared Rust/WASM core**, pin exact versions, deny known-vulnerable advisories, zeroize secret material where supported, and encrypt every raw-content frame before it reaches relay transport. Store the host static private key in the Windows credential vault; encrypt the browser's WASM device private key at rest with a non-exportable WebCrypto AES key held in IndexedDB.
- [ ] **Step 6: Prove relay opacity** by instrumenting the relay boundary and scanning captures/logs/database for fixture secrets while still observing only route/channel/size/timing metadata.
- [ ] **Step 7: Obtain independent security review** of the threat model, ADR, pairing UX, dependency state, key storage, and tests. Resolve all critical/high findings before public hosted use.
- [ ] **Step 8: Run** both crypto suites/advisory scans and commit as `feat(connect): add reviewed end to end channel`.

### Task 9.6: Add the minimal opaque hosted relay and routing tickets

**Files:** Portal API model/migration/routes/controller/services listed above, adjacent `.test.ts` files, `src/connect/relay.rs`, `tests/connect_realtime.rs`

- [ ] **Step 1: In isolated Portal API worktree, write failing tests** for authenticated host/device registration, short-lived single-use route tickets, route authorization, host offline, expiry, revocation, per-account/IP pairing limits, presence TTL, opaque binary relay, bounded frames/queues, and disconnect cleanup.
- [ ] **Step 2: Run** `npm --prefix C:\Code\happier\portal\api test -- src/services/devmanagerConnect/tickets.test.ts src/services/devmanagerConnect/relay.test.ts src/services/devmanagerConnect/presence.test.ts src/services/devmanagerConnect/rateLimit.test.ts src/routes/devmanagerConnectRoutes.test.ts` and `npm --prefix C:\Code\happier\portal\api run type-check`; capture the red results before migration/service code.
- [ ] **Step 3: Add minimal tables** for host public/routing identity, device public identity/revocation, route tickets, and ephemeral presence. Do not store raw task frames or cryptographic private keys.
- [ ] **Step 4: Issue short-lived signed routing tickets** only to authorized account/device/host relationships; validate origin/audience/expiry/nonce once at channel bind.
- [ ] **Step 5: Relay opaque binary frames** between one outbound host socket and authorized device sockets with per-channel quotas/backpressure. Logs contain route IDs, sizes, timing, status, and error class—never payload bytes/tokens.
- [ ] **Step 6: Implement host outbound connection** with reconnect/backoff/jitter, route ticket refresh, E2E handshake above relay, and local online/offline/degraded projection.
- [ ] **Step 7: Run** the Portal API tests from Step 2, `npm --prefix C:\Code\happier\portal\api run type-check`, `npm --prefix C:\Code\happier\portal\api run build`, and `cargo test --test connect_realtime hosted_relay_ -- --nocapture`; commit each repo independently as `feat(connect): add opaque realtime routing`.

### Task 9.7: Make alternating desktop/phone use invisible and race-safe

**Files:** `src/connect/{session,presence}.rs`, `src/kernel/command_bus.rs`, `web/src/connect/session.ts`, `tests/{connect_realtime,connect_permissions}.rs`, web tests

- [ ] **Step 1: Write failing tests** where desktop sends, phone sends five minutes later, desktop sends again, both type concurrently, both answer one question, one disconnects mid-command, a stale action arrives after provider restart, and local optimistic echoes reconcile in different orders.
- [ ] **Step 2: Run** `cargo test --test connect_realtime alternating_ -- --nocapture` and `npm --prefix web test -- connect`; save the red outputs.
- [ ] **Step 3: Treat each mutation independently** with `ClientId`, `CommandId`, `OperationId`, expected revision, and per-resource input sequencing. There is no persistent solo controller lease or visible owner badge; an accepted receipt means only that the operation is durable, not that its effect has settled.
- [ ] **Step 4: Track last valid sender only as ephemeral presence/UX metadata** for focus hints. It cannot grant permissions or block the owner's other device.
- [ ] **Step 5: Resolve questions/approvals through the kernel's atomic first-answer-wins rule.** Every actionable prompt carries `RequestId`, `ActionEpoch`, and provider runtime generation; the first matching response settles the request, every stale/duplicate response returns `AlreadyResolved` or `StaleAction`, and no rejected answer falls through to terminal input.
- [ ] **Step 6: Prove no refresh and no stale first-response acceptance** over alternating-send, reconnect, provider restart, sleep/wake, and background/foreground mobile cases; commit as `feat(connect): add invisible multi device handoff`.

### Task 9.8: Add task-scoped guests, Watchers, and Collaborators

**Files:** `src/connect/{invites,permissions}.rs`, `src/domain/{command,event,snapshot}.rs`, `src/config/remote_store.rs`, `web/src/connect/{invites,permissions}.tsx`, `tests/connect_permissions.rs`, web tests

- [ ] **Step 1: Write failing tests** for a task-only invitation credential, nickname, expiry, single-use/multi-use policy, capability bundle, Watcher read-only, Collaborator allowed commands, owner-only dangerous approval, revoked guest, closed Task, and no visibility into other Tasks, personal prompts, configuration, paired devices, or secrets.
- [ ] **Step 2: Run** `cargo test --test connect_permissions -- --nocapture` and `npm --prefix web test -- invites permissions`; record red results.
- [ ] **Step 3: Define a separate invitation store and protocol** keyed by `TaskInviteId`, secret hash, `TaskId`, role, allowed action/content classes, issuance/use policy, created/expiry/revoked timestamps, and the device public identity bound at redemption. Redemption uses the Task-invitation-labeled Noise XX path from Task 9.5, pins the same host identity, and never creates owner-device authority. It must not reuse or rotate the long-lived host pairing code. The local host always enforces the final decision.
- [ ] **Step 4: Keep collaboration UI hidden** until the owner creates an invite. Normal one-user operation shows only ordinary connection health/device settings.
- [ ] **Step 5: Render Watcher as realtime read-only** including presence and permitted content. Collaborator composer/actions derive from grant/capabilities; forbidden actions are absent or clearly disabled.
- [ ] **Step 6: Revocation closes active channels and invalidates queued commands** for that grant without revoking the owner's paired devices. Expiry/revocation leaves an auditable metadata event but no reusable plaintext secret; commit as `feat(connect): add task scoped collaboration roles`.

### Task 9.9: Add privacy filtering, push, and update continuity

**Files:** `src/connect/{projection,push}.rs`, `src/updater/mod.rs`, `web/src/pwa/*`, Portal relay/push services, `tests/connect_realtime.rs`, associated web/API tests

- [ ] **Step 1: Write failing tests** for Personal local-only, ManagedMetadata, RawContent grant, sanitized push, stale bundle, protocol incompatibility, desktop update, host reconnect, pairing-code stability, task-invitation expiry/revocation, device-key stability, and explicit manual pairing-code rotation.
- [ ] **Step 2: Run** `cargo test --test connect_realtime -- --nocapture`, `npm --prefix web test -- pwa connect`, `npm --prefix C:\Code\happier\portal\api test -- src/services/devmanagerConnect/relay.test.ts`, and `npm --prefix C:\Code\happier\portal\web test -- src/components/devmanager/ConnectFrame.test.tsx`; save red results.
- [ ] **Step 3: Classify every outbound field** before serialization; default deny unknown event fields. Personal Tasks never register metadata with Connect until deliberately enrolled.
- [ ] **Step 4: Make push payloads contain only** host/task opaque IDs, attention kind, safe title only when policy allows, timestamp, and route deep link. Never include prompt/response/terminal/browser/diff/file content.
- [ ] **Step 5: On bundle/protocol mismatch**, pause mutations, preserve local draft/device keys, prompt reload/update, reconnect with the same paired identity, resync, and resume. Do not rewrite `remote.json` or rotate codes.
- [ ] **Step 6: Run simulated old/new client/host/relay matrix** and commit as `feat(connect): enforce privacy and update continuity`.

### Task 9.10: Prove direct and hosted Connect under failure

**Files:** `scripts/native-next/Invoke-ConnectE2E.ps1`, `tests/connect_realtime.rs`, Portal API/web tests, `docs/connect-e2e-matrix.md`, `docs/replacement-deletion-ledger.md`

- [ ] **Step 1: Build the real E2E matrix:** direct LAN and hosted relay; desktop and phone; Chat/Terminal/Browser/Changes/Files/Services/Artifacts/Prompts; owner/watcher/collaborator; fresh/reconnect/resync/offline draft; paged/chunked snapshot interruption; update without re-pair; manual pairing-code rotation, task-invitation expiry/revocation, and device revocation.
- [ ] **Step 2: Add deterministic network shaping** for latency, jitter, packet/frame loss, bandwidth, reorder, relay restart, host sleep/wake, client background, and stale route ticket.
- [ ] **Step 3: Run fixture-provider input, exact-version prompt insertion, and browser automation** in the ordinary matrix. Only after explicit operator opt-in, repeat the selected low-volume cases with an explicit stock-provider allowlist from phone through the hosted relay while desktop observes, then reverse direction. Require sub-second semantic state under normal network and no manual refresh. Provider intelligence is not benchmarked; the proof checks protocol, lifecycle, rendering, authorization, and cleanup only.
- [ ] **Step 4: Capture relay logs/database/network boundary** and scan for seeded raw secrets/content; require no plaintext match. Verify rejected/tampered/replayed frames never reach the kernel.
- [ ] **Step 5: Simulate desktop/web version update** and prove the same pairing code, host identity, authorized device, device key, and Task subscriptions reconnect; separately prove existing unexpired task invitations keep their documented validity and are never regenerated by an app update.
- [ ] **Step 6: Close Tasks/full host and require zero** relay routes, local listeners where expected, provider/browser/tool processes, queued operations, and stale presence after TTL.
- [ ] **Step 7: Finalize shared conformance baseline/variant manifests** for direct/hosted routing, operation settlement, paging/chunk resume, unknown fallback, pairing versus Task invitation, role enforcement, prompt access, update continuity, and cleanup; resume one interrupted run and rebuild the index.
- [ ] **Step 8: Update the deletion ledger** for the old remote bridge/lease/presentation ownership; commit DevManager and Portal proof changes independently.

## Phase 9 verification gate

- [ ] Read each repo's local agent guidance and capture clean status/baseline in all DevManager/Portal worktrees.
- [ ] Capture production DevManager hashes/PID/start time and announce the long multi-repo/realtime gate.
- [ ] Run `cargo test --test connect_identity --test connect_crypto --test connect_session --test connect_permissions --test connect_realtime -- --nocapture`.
- [ ] Run `npm --prefix web test && npm --prefix web run typecheck && npm --prefix web run build`.
- [ ] In Portal API run `npm --prefix C:\Code\happier\portal\api test -- src/services/devmanagerConnect src/routes/devmanagerConnectRoutes.test.ts`, `npm --prefix C:\Code\happier\portal\api run type-check`, and `npm --prefix C:\Code\happier\portal\api run build`; in Portal web run `npm --prefix C:\Code\happier\portal\web test -- src/components/devmanager`, `npm --prefix C:\Code\happier\portal\web run type-check`, and `npm --prefix C:\Code\happier\portal\web run build`.
- [ ] Run dependency vulnerability/license checks for the selected crypto implementation and retain results with the independent security review.
- [ ] Rebuild and verify the immutable conformance index for direct/hosted, reconnect/resync, operation settlement, frame/chunk bounds, unknown-event fallback, pairing, task invitations, roles, and paired-owner prompt access.
- [ ] Run `pwsh scripts/native-next/Invoke-ConnectE2E.ps1 -Direct -Hosted -Mobile -FailureMatrix -UpdateMatrix -FixtureProviders`; run a separate authenticated provider arm only with explicit user authority and an explicit provider allowlist.
- [ ] Inspect relay opacity captures/logs/database and the full paired-device/task-invitation update matrix.
- [ ] Confirm no local/relay test/helper/provider/browser/Cargo/rustc process or route remains.
- [ ] Compare production invariants and review the complete diffs/deletion ledger in every repo.

## Phase 9 exit criteria

- Desktop, phone, and browser are live clients of the same Task; commands/events reconcile without manual refresh.
- One user alternates devices without a visible control lease, duplicate input, or accidental choice activation.
- Long-lived device pairing and task-scoped invitations are distinct credentials with distinct stores, scopes, expiry, rotation, and revocation behavior; updates rotate neither.
- Direct and hosted relay paths share the same commands, permissions, snapshots, replay, resync, and backpressure semantics.
- Hosted Connect can route/presence but cannot decrypt raw content; tamper/replay/revocation tests and independent review support that claim.
- Watchers and Collaborators are Task-scoped, realtime, locally enforced, expiring/revocable, and invisible until invited.
- Paired owner devices can search and insert exact personal prompt versions without creating a Connect-authoritative copy; invited guests cannot access the personal library.
- A full failure/update/mobile matrix settles with no orphaned local resources or stale relay routes.
