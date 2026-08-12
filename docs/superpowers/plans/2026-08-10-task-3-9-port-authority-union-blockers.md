# Task 3.9 Port Authority Union Blockers

> **For Codex:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task.

**Goal:** Make the production port/status/control path consume exact typed authority and fail closed for stale, incomplete, or non-authoritative observations while removing PID-only port control.

**Scope:** `src/process/ports.rs`, narrow port projection/control in `src/app/mod.rs` and `src/state/runtime_state.rs`, remote port DTO/predicate/web bridge, web port DTO/UI, platform lsof diagnostics, and focused tests. Do not touch CPU accounting or production/profile/session state.

**Design:** Treat every rendered/control-capable port row as a typed authority observation with independent freshness, listener identity, resource fence, membership generation, and control reason. The real app supplies no managed authority until the Task3.4 registry handoff exists; occupied rows therefore remain Unknown. Test seams construct exact live fences and independently fresh listener/membership snapshots. Refresh work is accepted only when task epoch, runtime/resource generation, and managed snapshot generation still match. Port controls are limited to typed authority decisions; legacy kill/restart APIs are removed.

## Tasks

- [ ] Add RED tests for exact managed-fence validation, independent freshness/membership reconciliation, failed/reap-incomplete suppression, refresh fencing, typed web projection, and path-free lsof diagnostics.
- [ ] Extend port authority/status data with observation/freshness/control metadata and exact live-fence validation; keep external classification fail-closed when registry authority is unavailable.
- [ ] Wire app refresh/action epochs and runtime authority through the real local/remote projection and terminal control paths; drop stale callbacks and expose typed reasons.
- [ ] Remove PID-only kill/restart tombstones and route remaining controls through typed authority.
- [ ] Extend remote/web DTO and UI to render green managed, blue proven external, orange starting, and Unknown for stale/fault/mixed states with redacted fixed diagnostics.
- [ ] Make post-launch settlement failures visible and exact-fence-bound without claiming foreign ownership; preserve Task3.4 dependency.
- [ ] Run focused RED/GREEN tests, Rust library/process/registry checks, web typecheck, fmt/diff/residue/hash/PID verification; commit only a clean bounded correction.
