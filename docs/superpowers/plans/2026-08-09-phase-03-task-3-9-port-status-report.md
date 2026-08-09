# Phase 3 Task 3.9 Port Status Report

Date: 2026-08-09

## Result

Task 3.9 now has a dependency-safe authority and inventory boundary in the
owned modules:

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
- `src/services/ports_service.rs` provides an immutable background
  `PortInventory`: one in-flight scan, latest-request coalescing, bounded
  waiters and ports, monotonic publication fencing, exact-result refreshes,
  cancellation, shutdown, absolute scan deadlines, and explicit timeout or
  probe-failure snapshots. It never probes from the read/render path.
- Listener enumeration captures PID creation identity and revalidates both the
  listener table and process identity. A change becomes a reconciliation fault
  and cannot authorize a free launch. Start admission accepts only a fresh,
  exact single-port `Free` proof and never adopts or kills an occupant.
- `src/services/platform_service.rs` preserves the exact PID-plus-creation
  primitive while exposing endpoint-rich Windows TCP tables and non-Windows
  listener parsing.

## Verification evidence

All Rust commands used
`CARGO_TARGET_DIR=C:\Temp\devmanager-phase39-fix`.

| Command | Result |
| --- | --- |
| `cargo test --test port_status -- --nocapture` | 36 passed, 0 failed |
| `cargo check --lib` | passed; 7 pre-existing unrelated warnings |
| `cargo fmt --all -- --check` | passed |
| `git diff --check` | passed |

The focused tests cover stale fence/member/revision, stopped/failed/leaked
generations, access denied, mixed and dual-stack endpoint authority, table and
PID-reuse races, fresh exact launch proof, refusal without listener mutation,
single-flight/coalescing, bounded queues, timeout/cancellation/shutdown,
uncooperative scanners, and stale late publications. Real temporary Windows
IPv4, IPv6, wildcard, and dual-stack listeners are used where supported; no
external process is killed.

## Remaining integration seam

This work intentionally does not edit `app/mod.rs`, sidebar, terminal,
`ProcessManager`, registry, remote, config, or DTO files. Host immutable
snapshot wiring, UI color/state projection, remote forwarding, and the real
managed Job listener remain deferred until corrected Tasks 3.4 and 3.7 supply
their live dependencies. Until then, compatibility projections must not be
treated as proof of managed ownership or as authority to control an external
listener.
