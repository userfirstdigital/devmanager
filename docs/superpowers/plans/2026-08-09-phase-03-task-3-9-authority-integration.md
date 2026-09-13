# Phase 3 Task 3.9 Authority Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:test-driven-development and superpowers:verification-before-completion for each task.

**Goal:** Keep exact listener authority, process identity, resource fences, and freshness intact through the native sidebar, remote snapshot/forwarding, and managed server start paths.

**Architecture:** The existing immutable `PortInventorySnapshot` remains the only listener observation source. The app will retain a typed per-port projection beside the legacy compatibility rows, derive local and remote UI/control decisions from the typed projection, and serialize a redacted fence-bearing authority DTO for remote consumers. Managed start admission will remain held through post-launch listener settlement; a foreign listener returns a safe bind-conflict result and only the exact managed child may be stopped. The legacy kill-port operation will be removed from the UI/process-op surface until Task3.4 supplies exact control authority.

**Tech Stack:** Rust, GPUI, serde/messagepack, portable-pty, Windows Job Objects, bounded native listener probes.

## Global Constraints

- Never use PID-only legacy `PortStatus` as managed or external authority.
- Preserve exact PID creation-time, canonical executable, resource fence, membership, generation, and freshness checks.
- Starting remains orange through probe failure; stale callbacks and changed port sets cannot publish.
- Remote forwarding accepts only a fresh managed authority DTO whose fence matches the live remote generation.
- No external listener may be killed, adopted, or treated as managed.
- Stock servers may bind after process spawn; logical reservation is not an atomic OS bind.
- Non-Windows lsof uses a pinned trusted executable path, bounded output/deadline, and redacted diagnostics.
- Do not touch production config/profile/session files or launch the installed application.

---

### Task 1: Typed local authority projection

**Files:**
- Modify: `src/process/ports.rs` authority classification and projection helpers.
- Modify: `src/app/mod.rs` `ServerPortSnapshotState`, refresh projection, authority construction, indicator derivation, and remote snapshot publication.
- Test: `src/app/mod.rs` unit tests and `tests/port_status.rs`.

**Interfaces:**
- `PortInventorySnapshot` remains the input.
- `NativeShell` retains rich `process::ports::PortStatus` values beside legacy `models::PortStatus` rows.
- Indicator derivation consumes rich authority and only falls back to `Unknown` for legacy rows.

- [x] Write RED tests proving a fresh typed external listener reaches blue, a typed managed listener reaches green only with an exact fence/member snapshot, stale/mixed membership becomes Unknown, and starting plus probe failure remains orange.
- [x] Run the focused app/port tests and confirm each fails because the real render path still consumes legacy rows or hardcodes Unknown.
- [x] Implement the typed projection and pass it through refresh publication, configured-port invalidation, and local indicator/terminal status consumers.
- [x] Run the focused tests to GREEN and verify old legacy rows remain fail-closed.

### Task 2: Remote authority DTO and forwarding validation

**Files:**
- Modify: `src/remote/mod.rs` snapshot/delta storage, publication, and host forwarding validation.
- Modify: `src/app/mod.rs` host publication and remote-client forwarding/indicator projection.
- Modify: `src/remote/web/dto.rs` only if the remote web wire surface needs the typed rows.
- Test: `src/remote/mod.rs`, `src/remote/web/dto.rs`, and focused remote tests.

**Interfaces:**
- Add a serde-safe `RemotePortAuthority` DTO carrying port, authority kind, optional `ResourceFence`, runtime generation, listener PID/creation identities, publication sequence, observation timestamp/deadline, and bounded error state; canonical executable paths never cross the wire.
- Legacy `port_statuses` remains backward-compatible but is never sufficient for forwarding.

- [x] Write RED tests proving managed forwarding succeeds only for a current exact DTO/fence and that PID-only, missing-fence, stale, mixed, or mismatched DTOs fail closed.
- [x] Run those tests and confirm current `runtime_owns_port`/legacy forwarding rejects or cannot distinguish the cases.
- [x] Implement DTO validation, snapshot/delta propagation, client projection, and host-side revalidation.
- [x] Run focused remote tests to GREEN and verify old snapshots deserialize as Unknown/non-forwardable.

Managed remote forwarding remains deliberately non-forwardable for live app publications until Task3.4 supplies the current ProcessRegistry membership/generation metadata; synthetic exact-fence DTO coverage proves the accepting seam without weakening that dependency.

### Task 3: Start reservation through post-launch bind settlement

**Files:**
- Modify: `src/services/ports_service.rs` reservation lifecycle helpers.
- Modify: `src/services/process_manager.rs` worker-side start/restart/restore/all-start admission seam.
- Modify: `src/process/ports.rs` settlement result/error types.
- Test: `tests/port_status.rs` and process-manager focused tests.

**Interfaces:**
- The worker owns the reservation from exact free proof through actual spawn and a bounded listener settlement window.
- Settlement compares exact spawned PID plus creation time and canonical executable against the listener table.

- [x] Write a deterministic RED foreign-race test where a foreign listener appears after free proof; assert safe bind conflict and no foreign kill.
- [x] Write RED coverage proving reservation remains active while the worker callback is blocked and releases on both bind success and failure.
- [x] Implement post-launch settlement and safe cleanup of only the exact managed child; do not claim atomicity for stock servers.
- [x] Route local, remote, restart, restore, and start-all paths through the same worker seam, then run focused GREEN tests.

### Task 4: Membership reconciliation and dead control removal

**Files:**
- Modify: `src/services/process_manager.rs`, `src/process/registry.rs`, and `src/app/mod.rs` for authoritative membership reconciliation.
- Modify: `src/services/process_ops.rs` and callers/tests to remove the unconditional legacy kill-port operation.
- Test: `tests/process_supervisor/registry.rs`, `tests/port_status.rs`, and app/process-manager tests.

- [x] Write RED tests proving listener and membership snapshots from different generations cannot produce blue/green, and stale control fences cannot restart or terminate a process.
- [x] Remove the dead `KillPortAndRestart` operation/UI action unless exact authority is available; preserve ordinary exact-fenced managed stop/restart.
- [x] Implement a second authoritative membership reconciliation seam immediately before authority publication; fail closed when the Task3.4 membership handoff is unavailable.
- [x] Run focused process/registry tests and verify no unconditional `kill_port` call remains on a reachable path.

### Task 5: Non-Windows probe hardening and report correction

**Files:**
- Modify: `src/services/platform_service.rs` trusted lsof path, bounded child output, timeout, and redacted diagnostics.
- Modify: `docs/superpowers/plans/2026-08-09-phase-03-task-3-9-port-status-report.md` integration/test scope and dependency wording.
- Test: platform-service tests and report checks.

- [x] Write RED tests for pinned executable selection, output/deadline bounds, macOS-native executable identity, and path-free diagnostics.
- [x] Implement the smallest platform-correct changes without changing CPU accounting.
- [x] Run platform-focused checks, update the report to the actual 50-test/integration scope, and review the complete diff.

### Task 6: Final verification

- [x] Capture production `config.json`/`remote.json` hashes and installed PID/start time without reading `session.json`.
- [x] Run focused RED/GREEN tests, full `port_status`, focused registry/process tests, `cargo check --offline --lib`, and `cargo fmt --all -- --check` with `CARGO_TARGET_DIR=C:\Temp\devmanager-phase39-port-correction3` and `CARGO_BUILD_JOBS=1`.
- [x] Confirm no Cargo/rustc/lsof/test/worker residue and unchanged production hashes/PID.
- [x] Run `git diff --check`, inspect the complete diff, commit the correction, and report the honest Task3.4 dependency.
