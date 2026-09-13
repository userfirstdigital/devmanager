# Remote Wave 3: Explicit LAN setup and host-scoped native clients

Status: local setup control and native settings source-integrated. Native Connect
client and device public-key vault are integrated; grouped verification is in
progress. Native fleet UI, canonical enrollment bridge, and real-device acceptance
remain pending. This is not complete seamless-remote acceptance.

## Prerequisite

The current union must pass the isolated all-target Rust check and targeted real
Noise, semantic wake, custody, receipt-recovery and TLS lifecycle checks. Keep
one compiler-tail owner. Freeze all web source, including tests, before building
the embedded bundle: its fingerprint deliberately covers the entire web tree.

## Local setup control

- Add a global, host-local-only Remote Access request/result beside Provider
  Settings. Deny it at Connect permission admission; a remote browser cannot
  enable listeners, replace trust, export private keys or reconfigure its host.
- Native Settings consumes this global response, not a task-filtered result.
  Read-only status never starts a listener or establishes identity.
- The durable host owns a bounded control mailbox and status/operation board.
  The executor admits an operation without awaiting listener shutdown/start.
  Host lifecycle code owns and joins the exact controller on reconfiguration.
- Explicit Enable establishes the existing canonical identity with
  `OsConnectHostVault` and one retained exact `IdentityCommand`. Only after
  custody is committed does it persist a narrow remote configuration patch and
  start the listener. A committed identity is never rolled back to hide a later
  bind failure. Pending transitions after process death need explicit recovery;
  do not fabricate the lost command timestamp or add another identity journal.
- Disable stops and joins first, then persists disabled state; identity and
  paired devices remain. Reload is explicit and loads keys without minting.
- Reuse the existing atomic remote-config writer, preserving unrelated fields
  and secrets. UI sends editable fields, never the entire config or private key.
- LAN browser setup requires actual trusted HTTPS/WSS. Show the exact endpoint,
  certificate/trust requirements and pairing action. Do not silently change a
  firewall, OS trust store, public listener or Connect deployment.

## Pairing identity

The currently bounded browser cookie-to-Noise pin is a bootstrap binding, not a
canonical `DeviceId`. Before exposing canonical device management or WAN tickets,
bind that association durably through the existing identity store. Never cast a
legacy cookie/client string to a device ID. Revocation must wake and close active
channels and retire queued authority, not merely remove a row from the UI.

The vault now implements device public-key registration slots. The remaining
production bridge must connect that existing vault
with a public-key registration slot, bound only from a successfully authenticated
Noise device peer. Preserve the peer's authenticated Host/Device claim kind;
never treat a Host claim as device enrollment. The client keeps the private key.
The host stores the peer public key and exact nonce/slot-bound establishment
state, and the existing IdentityStore creates the canonical DeviceRecord and
receipt. No second identity journal or custom signature protocol. Retain the
store's claim owner across a failed mutation and its exact retry. A logical
revision advance is admissible only with that command's durable matching receipt.

## Live integration findings

- Real isolated host + browser pairing and encrypted task snapshots work.
- Production bootstrap exposed an XX optional-pin constructor mismatch and a
  route/connection ID mismatch; both were fixed in the shared production path,
  and the real-WASM fixture now includes the complete host publication.
- Two browser viewers rendered a real Codex exchange and its provider session
  ID. The initial SendNow needed a separate Enter: physical write settlement was
  not provider execution. The corrected Codex submit uses Herdr's complete
  bracketed-paste + separate Enter shape plus the provider settle window below.
- The phone terminal button queries the existing runtime-fenced terminal screen
  on demand. Fixed keys use the canonical outbox/receipt path; a real Codex trust
  prompt was answered before its first conversation turn. Arbitrary raw terminal
  input and durable terminal caching remain outside that control row.
- The native transport reuses ClientConnection/HostClient, keeps local Hello
  metadata distinct, and preserves conversation dirty notifications. Its real
  HTTP pairing/reconnect smoke passes; no desktop fleet UI is implied.
- The complete paste preserved the exact prompt but still needed another Enter.
  Codex's 120ms Enter-suppression window exceeds the old 50ms delay. The new
  provider-specific 250ms settle passed the real single-Send test: both viewers
  displayed the exact prompt and SINGLE_SEND_REMOTE_OK without another Enter,
  with a correlated provider session ID.
- The production browser/device enrollment path passed a same-owner restart:
  after one Send produced `RESTART_OWNER_OK`, the exact isolated host/provider
  tree was joined and restarted with the same profile and keys. The open browser
  recovered without re-pairing, retained history and `KEEP_DRAFT_AFTER_RESTART`,
  and a follow-up asking for the previous reply produced `RESTART_OWNER_OK`.
  The correlated provider session ID remained
  `01a04408-8d2f-77e0-b201-046cf5d75656`. This is loopback process-restart evidence,
  not physical LAN, phone sleep/wake or WAN acceptance.

## Native host ownership

- Retain one authenticated client/model per explicitly trusted host.
- Every task, selection, pane cache, query and send carries `(hostPublicId,
  taskId)`. Identical task UUIDs on different hosts must remain distinct.
- Freeze the owner at submit admission; focus changes cannot retarget a write.
- A disconnected owner remains visible with cached content and offline status.
  Never attach its provider, path or workspace to the local executor.
- Reuse the current recursive split workspace, command bus, native semantic
  projection, receipt queries and Connect byte carriers. Do not create a second
  task database or infer conversation identity from paths/transcript order.
- Discovery supplies endpoint hints only. Initial trust is explicit; subsequent
  discovery and reconnect use the pinned host identity automatically.

## Acceptance

Two isolated hosts, two clients and browser UI exercise task discovery, live
updates, concurrent viewing, exact-owner sends, owner outage, same UUID on both
hosts, restart, cache-first return and revoke. Keep simulated/loopback results
separate from real PC/phone LAN results. WAN is the following wave, carrying the
same authority/commands over the existing Connect relay, with no cloud task owner.
