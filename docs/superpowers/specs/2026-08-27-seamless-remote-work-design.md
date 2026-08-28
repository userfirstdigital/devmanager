# Seamless remote work

## Product contract

One inbox spans explicitly trusted computers. A task always runs on its owning
computer; viewing it, changing focus, or changing network does not migrate or
restart its provider. Each device keeps its own pane layout, selection and drafts.
Projects remain multi-folder workspaces on the owning computer.

LAN use must not require the hosted Connect service. Outside-home use transports
the same authenticated commands, receipts, snapshots and events through Connect.
The relay is not a task database or execution authority. The current service to
integrate is `C:\Code\userfirst\connect`, superseding the historical Portal paths
in the August 4 phase-9 plan. No deployment, firewall change or public listener is
part of source-level implementation acceptance.

Phone return restores the last validated screen immediately and reconciles in
place. A small Syncing/Offline indication is truthful; a full-screen Connecting
replacement is reserved for a device with no cached state. Mobile operating
systems can suspend a socket, so uninterrupted background networking is not the
contract. No manual refresh, re-pairing or provider restart should be necessary.

## Current evidence, not assumed readiness

Audited source at `fbf8907`, August 27, 2026; no multi-device live acceptance yet.

| Surface | Reuse | Remaining production gap |
| --- | --- | --- |
| Durable host/client | Command IDs, receipts, event journal, snapshots, replay, exact provider identity | Native shell attaches only one local named-profile host |
| Connect Rust/WASM | Fixed Noise XX/IK, sealed frames, replay protection, OS host key custody | Browser bootstrap always holds on a non-extractable X25519 key incompatible with the byte-oriented WASM ABI |
| Direct web bridge | Pairing, cookies, rate limits, encrypted request dispatch | No host output stream; cookie-only connection identity does not bind the durable device key |
| Existing mobile UI | Semantic messages/tools/questions, drafts, safe rendering, PWA lifecycle | Legacy commands are rejected by encrypted mode; `/tasks` resume routes are rejected by the old `/session` validator |
| LAN | TLS and direct admission primitives | Raw listener does not implement the TLS contract, discovery and trusted endpoint provisioning |
| Connect service | Authenticated one-use route tickets, opaque WebSocket relay | Native ticket acquisition/outbound routing absent; active relay revocation not wired |
| T3 | Environment registry, cache-first render, bounded replay, wake probes, stable outbox IDs | Selective reference, not a replacement kernel; no automatic discovery |
| Traycer | Immutable host-bound tabs, shared host connections, frozen submit target | Public snapshot omits host/backend and sent-command deduplication |

Do not revive the legacy plaintext mutation bridge to make a demo work. Do not
claim exported functions or fixture tests establish product reachability.

## Architecture decisions

1. **Canonical owner.** Address every remote artifact by `(hostPublicId, taskId)`.
   Resolve host before reading a task; freeze that owner with every submission.
   A disconnected owner never falls back to the local PC. Provider credentials,
   worktrees, paths and `providerSessionId` remain on the owner. Preserve existing
   generation/action-epoch checks and first-answer-wins approvals.
2. **One connection per host per client.** Keep authenticated sessions outside
   view components. Switching tasks changes subscriptions, not network ownership.
   Hide or close a viewer without terminating its provider. One host's timeout
   must not block another host's inbox, send lane or reconnect.
3. **One wire contract.** Reuse `ConnectEnvelope`, `ConnectDispatchSession`,
   `ConnectHostCommandPort`, `HostClient`, and the durable output registries.
   Direct WSS and relay are byte carriers. Neither adds a second CommandBus or
   converts execution into legacy PTY actions.
4. **Race-free snapshot/live handoff.** Attach bounded output before capturing
   the snapshot boundary. Install the snapshot atomically, replay after its
   committed sequence, then drain live events in order. Duplicate sequences do
   not duplicate messages; holes, expiry, overflow and host changes force a
   fresh bounded snapshot. A physical socket write, not queue admission, advances
   a delivered cursor. Critical receipts and durable facts retain their ordering.
5. **Reliable commands.** Persist outbound command ID and owning host before
   transmission. A dropped acknowledgment is Uncertain, not Failed and not a
   fresh command. Reconcile using the same ID and host receipt. A new provider
   generation invalidates old input; it cannot silently create a conversation.
6. **Cache-first clients.** Persist a bounded, versioned semantic projection and
   committed replay cursor per host/task. Keep drafts separate from incoming
   projections. Cache is presentation-only and never grants action authority.
   Validate account/device/host/schema before restoring it, reconcile on return,
   and clear the relevant host cache on unpair/revoke. Do not cache raw terminal,
   file bodies, credentials, approvals-as-authority or arbitrary API responses.
   This deliberately supersedes the old no-durable-semantic-cache mobile policy.
7. **Foreground recovery.** Retain current views; coalesce focus, visibility,
   pageshow and online into one recovery lane. Probe short absences; after a long
   suspension replace a stale socket and replay. Ignore late callbacks from old
   connection generations. Reset retry backoff only after authenticated readiness,
   not TCP open. No repeated polling loop is needed for normal chat updates.

## Identity and transport security

Reuse the fixed Rust/WASM Noise implementation. The approved phase-9 custody
design encrypts WASM device-private bytes at rest with a non-extractable
WebCrypto AES-GCM wrapping key stored in IndexedDB. The currently implemented
non-extractable X25519 object cannot be passed into Snow's synchronous ABI.
Correct that wiring without writing a JavaScript Noise implementation: unwrap
only for Rust/WASM handshake construction, zero temporary byte buffers, persist
only authenticated ciphertext/public metadata plus the non-extractable AES key.
Existing opaque v1 identities must not be silently replaced; surface explicit
repair/re-pair. Bind ciphertext to identity/version metadata using authenticated
additional data. Persisted records survive ordinary host and bundle updates.

First pairing pins the verified host identity and binds the device's static key
to its durable device record. Subsequent connections validate that same key and
current revocation generation. A routing ticket or web cookie is not a substitute
for Noise device authentication. Revocation closes active channels and invalidates
queued authority, not merely future tickets.

LAN discovery advertises identity/endpoint hints, never grants access. Initial
pairing is explicit. Direct browser LAN requires WSS with an appropriately trusted
certificate and exact Origin checking. Never trust arbitrary Forwarded headers
or an Origin string as proof of TLS. Loopback may use a browser-trustworthy HTTP
origin. No insecure LAN or click-through certificate fallback. Certificate setup
must be explicit and the UI must explain what a device needs to trust.

WAN obtains short-lived route tickets from Connect, establishes an outbound host
socket, and carries the same pinned E2E session. Direct-to-relay switching changes
only transport generation; task, device, command and replay identity remain stable.
Independent security review and opacity/revocation tests remain release gates.

## Phone and desktop presentation

- Unified inbox: host and project metadata, active tasks first, compact Done
  section at the bottom, archive separate, floating search/new task on phone.
- Phone task: stable back/title/project/host header, compact Files/Changes/Terminal
  actions, full-width semantic chat, bottom composer with safe-area/keyboard fit.
- Cached content remains visible while Syncing; sending is gated by current host
  authority. Offline drafts remain editable. Uncertain sent input is not silently
  retransmitted with a new identity.
- Desktop remote tasks use the existing recursive split workspace. Focused and
  unfocused views subscribe to the same owner-host projection and keep updating.
- Pair once, then discover/reconnect automatically. Host-offline state explains
  that execution belongs elsewhere; it never offers an implicit local clone.

## Implementation waves and acceptance

1. Browser custody/bootstrap and foreground route continuity, using the existing
   WASM/transport and mobile lifecycle. Do not publish a usable encrypted client
   until the canonical projection adapter and host identity binding are ready.
2. Host-owned duplex Connect adapter and exact device identity. Exercise real
   host snapshots, replay, provider deltas, commands, cancellation and slow peers.
3. Canonical mobile task adapter/cache/outbox. Connect real task routes and
   composer to the host contract; remove legacy-action dependence.
4. LAN listener/settings/pairing and native multi-host registry. Test two distinct
   isolated hosts, including same task UUID on different hosts and owner outage.
5. Connect ticket/relay production integration and active revocation in the
   separately versioned service. No cloud task authority or plaintext content.
6. Consolidated gates and device acceptance. Source completion, local simulated
   transport proof, real LAN device proof and public WAN proof are separate states.

Acceptance includes simultaneous desktop/phone send, lost receipt, reconnect with
no duplicate input, sleep/wake, host restart, revoked device, out-of-order/overflow
events, stale browser bundle, multi-host failure isolation, and same conversation
identity after every permitted resume. Measure cached first paint, authenticated
catch-up, and host-event-to-render separately; do not confuse provider think time
with transport latency. Actual LAN/WAN performance requires real devices/networks.

## Reference reuse

- T3 (MIT): `packages/client-runtime/src/{connection,state}`, mobile
  `connection/environment-cache-store.ts`, `connection/app-state-wakeups.ts`,
  `state/thread-outbox-*`, and server `ws.ts` snapshot/replay handoff.
- Traycer (MIT): `clients/shared/host-client/host-connection-registry.ts`,
  host selection authority, and frozen composer placement.
- Herdr (Apache-2.0): server-owned client attachment and bounded reliable-control
  versus coalesced-render lanes. Not its SSH-only transport as a phone solution.
- DevManager phase-9 plan and ADR 0002 remain the crypto/protocol authority except
  for the explicitly superseded service location and semantic-cache policy above.

Preserve required license attribution for any copied/adapted source.
