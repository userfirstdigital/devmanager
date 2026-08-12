# Phase 3 Task 3.9 Port Status Report

Date: 2026-08-10

## Result

Task 3.9 now has a dependency-safe authority and inventory boundary through
the native app, remote host, and managed server start paths:

- `src/process/ports.rs` classifies `Managed`, `ProvenExternal`, `Unknown`,
  `ProbeError`, and `Free`. Managed ownership requires the exact
  `ResourceFence`, a current `Starting`/`Running` generation, exact
  PID-plus-creation member identities, and a fresh valid membership contract.
  Stopped, failed, leaked, stale, failed, mixed, and PID-reused evidence stays
  fail-closed.
- Registry projections carry the exact fence, lifecycle, member identities,
  membership revision, observation sequence/time, validity, and freshness
  contract. Unknown Job members invalidate ownership confidence.
- Listener observations retain TCP protocol, address family, bind address, and
  exact listener identity. IPv4/IPv6, wildcard, dual-stack, and mixed managed
  plus external endpoint evidence remain distinguishable.
- `src/app/mod.rs` keeps typed authority beside the legacy compatibility rows.
  Sidebar indicators consume the typed projection, stage both projections
  before refreshes, preserve orange `Starting` through probe failure, and can
  show blue only for a fresh identity-proven external listener. Legacy rows
  remain display-only and cannot authorize ownership, URL opening, or control.
- Remote snapshots and deltas carry a redacted `RemotePortAuthority` DTO with
  resource generation, listener PID/creation evidence, membership/observation
  sequence, publication timestamp, deadline, and bounded error state. Remote
  forwarding and remote Managed indicators require the exact fresh fence; old
  PID-only snapshots are non-forwardable. The host currently publishes zero
  membership metadata for managed rows because the live ProcessRegistry
  handoff belongs to Task 3.4, so that path intentionally remains fail-closed.
- `src/services/ports_service.rs` provides an immutable background
  `PortInventory`: one in-flight scan, latest-request coalescing, bounded
  waiters and ports, monotonic publication fencing, exact-result refreshes,
  cancellation, shutdown, absolute scan deadlines, and explicit timeout or
  probe-failure snapshots. It never probes from the read/render path.
- Listener enumeration captures PID creation identity and revalidates both the
  listener table and process identity. A change becomes a reconciliation fault
  and cannot authorize a free launch. Start admission accepts only a fresh,
  exact single-port `Free` proof and never adopts or kills an occupant.
- Worker-side starts and restarts acquire the shared reservation after queue
  admission and hold it through spawn plus bounded post-launch settlement.
  Exact owned listeners settle successfully; proven foreign races return an
  EADDRINUSE-style error and stop only the exact launch session; unverified
  listeners are stopped only through that exact session fence. A stock server
  bind is not represented as atomically reserved.
- Membership reconciliation has an explicit second-read seam. If the two
  registry observations differ, the projection retains listener evidence but
  returns `Unknown` rather than painting green or blue.
- `src/services/platform_service.rs` preserves the exact PID-plus-creation
  primitive while exposing endpoint-rich Windows TCP tables and non-Windows
  listener parsing. Non-Windows lsof is invoked by a pinned absolute path with
  bounded stdout/stderr and deadline; canonical executable paths are omitted
  from wire/diagnostic text.
- The legacy kill-port/restart operation is a tombstone until Task 3.4
  supplies an exact managed resource fence; it returns an explicit error and
  never calls the unconditional PID-only kill path.

## Verification evidence

All Rust commands used
`CARGO_TARGET_DIR=C:\Temp\devmanager-phase39-port-correction3` with
`CARGO_BUILD_JOBS=1` and `--offline`.

| Command | Result |
| --- | --- |
| `cargo test --offline --test port_status -- --test-threads=1` | 52 passed, 0 failed (the prior matrix was 50; two focused authority-race regressions were added) |
| `cargo test --offline --test process_supervisor -- --test-threads=1` | 39 passed, 0 failed |
| focused lib authority/refresh/settlement tests | passed (authority 22, refresh 11, stale typed authority 1, post-launch settlement 2) |
| `cargo check --offline --lib` | passed; only unrelated baseline warnings |
| `cargo fmt --all -- --check` | passed |
| `git diff --check` | passed |

The focused tests cover stale fence/member/revision, stopped/failed/leaked
generations, access denied, mixed and dual-stack endpoint authority, table and
PID-reuse races, fresh exact launch proof, refusal without listener mutation,
single-flight/coalescing, bounded queues, timeout/cancellation/shutdown,
uncooperative scanners, stale late publications, typed native/remote UI
projection, exact remote forwarding DTO validation, and deterministic
post-launch foreign/unverified settlement. Real temporary Windows IPv4, IPv6,
wildcard, and dual-stack listeners are used where supported; no external
process is killed.

## Remaining integration seam

Task 3.4 remains the explicit dependency for the live ProcessRegistry/managed
membership handoff and any transferable bind authority. Until it supplies
those values, managed remote forwarding and managed green/control projections
remain fail-closed; exact external blue evidence and safe start settlement are
still available. Task 3.9 does not change CPU accounting or touch production
profile/session state.
