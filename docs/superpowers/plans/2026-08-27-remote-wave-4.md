# Remote wave 4: shared desktop ownership and existing Connect carrier

Historical wave checkpoint: native trust, opaque-key workspace persistence and
fleet driver integrated; real two-host encrypted transport acceptance passed.
Phone multi-host admission and controls were integrated in subsequent work;
see `docs/REMOTE_MOBILE_WEB.md` for the current evidence. Native fleet UI,
LAN discovery and WAN reachability remain unfinished; wave 5 owns native integration.

## Verified foundation checkpoint

- The production native pairing path completed HTTP admission, Noise and Hello,
  synchronized a real isolated host, reopened its DPAPI-protected trust store
  from disk, and reconnected with the same assigned client identity. This is
  loopback transport proof, not physical LAN or WAN acceptance.
- The grouped checkpoint passed 598 remote tests, 3 interrupted-registration
  recovery tests and 64 recursive workspace tests. The main-source
  `cargo check --locked --lib --bins --tests` passed before keyed persistence
  was integrated; the final union gate still needs to run.
- Keyed persistence preserves the raw-TaskId API aliases while adding layout
  v6 and draft v2 loading/saving with explicit local-owner migration. Its first
  focused keyed run passed 7 tests, including same-TaskId/different-owner
  isolation and v1/v5 layout migration. Subsequent storage-bound corrections
  and the full persistence regression group remain in the final gate.
- Verification left the installed DevManager PID/start time and production
  `config.json`/`remote.json` hashes unchanged. All smoke host processes exited.
- The fleet checkpoint passed 18 lifecycle/ownership tests, 5 remote worker
  tests and 9 keyed-storage tests. The subsequent grouped client run passed
  156 tests, including same-poll disconnect/factory fencing, shared transport
  health, trust persistence and typed query deadlines. Fleet-port/native UI
  integration still requires its own final union gate.
- The unattended `remote-fleet-smoke` used two actual isolated host processes,
  production HTTP pairing, Noise/Hello and persisted native trust. Both hosts
  deliberately shared one raw TaskId. Owner-specific rename, B availability
  during A's outage, A's same-profile reconnect without re-pairing, stale A
  action rejection, and removal of A without disturbing B all passed. This
  does not prove the native UI or physical LAN/WAN behavior.
- Shared typed query helpers now preserve existing capability checks and long
  cockpit/agent deadlines. Global host queries no longer invent a task ID.
  Fleet adapter and native-shell wiring are the next integration boundary.

### Fleet adapter and cross-origin server checkpoint

The integrated `FleetClientPort` and B-side cross-origin pairing/prelude passed
`cargo check --locked --lib --bins --tests` on August 27 (isolated target,
`devmanager-fleet-cross-origin-alltarget3.log`). The native runtime candidate,
phone multi-host candidate, authenticated A-side roster/CSP and real TLS fixture
are not part of that compiler result. A-side publication and phone lifecycle
corrections remain in separate source lanes. The installed PID/start time and
both production configuration hashes stayed unchanged.

The real `remote-cross-origin-smoke` subsequently passed over actual rustls
HTTPS/WSS with a process-local ephemeral CA: same-origin owner pair, origin-bound
grant, credentialless phone pair, DMCX1 ticket, pinned XX/Hello, canonical task
snapshot, new-socket resume with the same assigned ClientId, one-use ticket
rejection and wrong-Origin rejection. The fixture host exited and its temporary
profile was removed. No OS certificate trust changed and no provider was started.
Log: `C:\Temp\devmanager-cross-origin-live1.log`. This is loopback protocol proof,
not browser or physical-device acceptance. Paired-client revocation is not yet
exposed by `RemoteSetupRequest`, so this fixture did not test live revoke.

Phone review found and queued corrections for hydrate/foreground ordering,
owner-scoped composer state, stable active watches, unknown-owner deep links,
first-pair/retry reachability, bounded pair-response body reads, and a protocol
mismatch: the existing production browser listener uses pinned Noise XX on every
connection, so the browser must not switch to IK on resume. Cached service-worker
navigation must also fetch fresh authenticated HTML/CSP when online and retain
the last-good private shell for offline presentation. None of these are claimed
as verified phone behavior yet.

## Phone multi-host admission boundary

T3's environment registry connects independently to multiple owners using
device-bound authorization and short-lived WebSocket tickets. The current
DevManager phone entry instead admits its same-origin cookie before Noise.
Porting only T3's registry would not permit safe cross-origin host connections.
Cross-host pairing, exact Origin admission and resumable device authentication
must be implemented together while retaining DevManager's pinned Noise identity
and encrypted browser custody. Do not add permissive credentialed CORS or fall
back to a plaintext/legacy bridge to make the merged inbox appear connected.

## Native integration contract

- `HostId::LocalProfile(String)` identifies the explicitly selected local host
  profile; `HostId::Remote([u8; 16])` identifies an authenticated pinned host.
  `HostTaskKey { host, task_id }` is presentation identity. Wire `TaskId` stays
  unchanged; never hash/remap it into a fabricated UUID.
- A `HostFleet` retains one host handle, client, canonical subscription/model,
  cached projection and monotonic connection generation per owner. No global
  active-host fallback is allowed in action execution. Removal/re-addition may
  not reuse a generation and revive queued work.
- Capture owner, generation and assigned client identity at action admission.
  Validate all three again at execution and outcome application. Snapshot,
  replay and conversation-dirty state remain per host, not merged domain maps.
- Reuse `NativeHostClientRuntime` workers and existing command/query handling.
  Do not build a parallel command bus. The shell merges validated rail views;
  it does not merge durable `ClientModel` IDs or start remote providers locally.
- Generalize the existing recursive workspace and surface registry to opaque
  task keys. Keep local aliases source-compatible; preserve all pane/split IDs,
  allocation/pin rules, project multi-folder metadata, selection and scroll.
  Native persistence subsequently needs an explicit schema migration from
  local-only keys using the layout store's exact local profile.
- Remote unsupported raw terminal, host update/shutdown and detach controls
  remain explicitly disabled. Semantic provider input and fixed terminal-key
  commands are separate capabilities from an interactive raw PTY channel.

## Existing WAN service, verified source contract

Source authority: Connect API `64f5d1a09a4b0fcf70fdd4aad740c88d322cbd12`,
`src/services/devmanagerConnect/authFrame.ts`, `tickets.ts`, and
`src/websocket/devmanagerConnectSocket.ts`. No service mutation/deployment made.

- Authenticated POST `/api/devmanager/connect/route-tickets` issues a one-use
  ticket for a service host/device enrollment. Service host IDs are not native
  Noise host public IDs; require an authenticated mapping, never a cast.
- The first binary WebSocket frame is exactly `DMCT`, big-endian u16 UTF-8 route
  length and route, then u16 token length and token. Each field is nonempty and
  at most 256 bytes. It is a standalone authentication frame with no trailing
  Noise/application payload. Never log the frame or place its token in a URL.
- WebSocket paths are `/api/devmanager/connect` and `/devmanager/connect`.
  The relay pairs at most two peers and forwards bounded opaque frames (256 KiB).
  The existing Rust `SignedRouteTicket::encode` is a different historical wire
  format and cannot be passed to this deployed-service path.
- After ticket redemption, carry the same host greeting, Noise, Hello, canonical
  requests and replay over the byte carrier. The relay must not terminate E2E
  encryption or become a task database.
- Host outbound ticket credentials, active revocation, route expiry/renewal and
  public WSS serving need explicit integration and tests. Issuance alone is not
  a working WAN connection. No firewall, trust-store or deployment changes are
  implicit in source implementation.

### Confirmed service gaps

The existing service authenticates user/device ticket issuance but has no
outbound native-host control authentication or route-offer delivery. Its portal
host UUID and Ed25519 enrollment identity are distinct from the native Noise host
UUID/key. Add an authenticated mapping to the existing host record; reuse that
enrolled signing key for a domain-separated host control challenge/claim, never
reuse the one-time enrollment bearer as a permanent credential. Mint the host's
second one-use route ticket atomically when it claims a route; stored hashes
cannot recover a previously issued bearer after restart.

The relay is process-local and needs connection affinity for both peers. Live
revocation must close redeemed sockets by host/device as well as revoke unused
tickets. No shared relay backplane or active revocation exists in the reviewed
service. These are implementation and deployment gates, not configuration toggles.

The current relay also discards an opaque frame when only its sender is bound:
`OpaqueRelay::forward` returns zero deliveries, and the WebSocket caller ignores
that result. Auth-in-flight frames are flushed after the sender's redemption,
not after the second peer arrives. The WAN integration must provide a bounded
two-peer rendezvous (including host-first and browser-first ordering) before
releasing the greeting/Noise frames. Never compensate by replaying arbitrary
encrypted frames or silently retrying commands. Preserve an absolute rendezvous
deadline and close both sides when the route expires or either identity is
revoked; do not retain an orphan peer until its ticket happens to expire.

## Acceptance

Same raw task ID on two owners must yield independent panes, drafts, histories,
receipt tracking and actions. A remote send followed by local focus remains
remote. One owner's outage/reconnect cannot block or invalidate other hosts.
Cached presentation survives disconnection but grants no write authority.
Run a real two-host encrypted route, then physical LAN devices, then public WAN;
keep these evidence levels separate.
