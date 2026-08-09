# Phase 6 Task 6.7a: Service Contract Report

Status: PARTIAL 6.7a. This batch implements the dependency-safe configured-service contract only. It emits validated plans and redacted evidence; it does not launch processes, probe live ports, or modify ConfigStore, terminal ownership, UI, or installed-app state.

## RED / GREEN

- RED: `cargo test --test service_supervisor -- --nocapture`, with `CARGO_TARGET_DIR=C:\Temp\devmanager-phase67a-20260809`, failed at the missing `services::model` and `services::health` imports.
- GREEN: the final focused run passed 11/11 tests, covering validation bounds and redaction, deterministic dependency/cycle handling, fake-clock startup/success/failure/stale/crash behavior, reducer states, dependency-aware start/stop/restart admission, duplicate start coalescing, external-port refusal, dependent failure refusal, stale fences, manual stop, and exact task-close ownership.
- Formatting: `rustfmt --edition 2021` passed for every owned Rust file.
- Existing unrelated warnings remain in `src/kernel/mod.rs` and `src/host/connection.rs`; no service-contract warning was introduced.

## Delivered contract

- `ServiceDefinition`, `LaunchIntent`, bounded command/cwd/env-reference/port/policy values, task/host scope, and explicit validation errors reject empty/duplicate/self/unknown dependencies, cycles, unsafe paths, raw secret-shaped values, and count/text overflow.
- `ServiceCatalog` produces deterministic dependency-first launch plans and dependent-first stop plans. It never creates an effect for an external observation.
- `HealthTracker` consumes scheduled fake-clock observations only, enforcing startup deadlines, probe intervals, bounded exponential backoff, consecutive success/failure thresholds, stale evidence, cancellation, process exit, and generation fences.
- `reduce_service` keeps lifecycle, process, health, port, and ownership axes separate. External occupied ports project blue `External` and admission rejects Stop/Restart/Start control for them. Running without fresh health evidence does not project `Healthy`.
- Typed admission decisions cover Start, Stop, Restart, coalesced duplicate operations, dependency readiness/order, exact task-owned close plans, and stale generation/epoch rejection.
- `RedactedServiceSnapshot` and `RedactedDiagnostic` expose bounded timestamps, evidence kinds, and provenance without command lines, environment values, or arbitrary output.

## Later blockers / out of scope

1. ConfigStore resolution and environment layering must later produce these validated definitions without materializing secrets into this contract.
2. Phase 3 Job-owned `TerminalService` supervisor/effects must consume plans and attach real process ownership.
3. Real background health probes and port/process ownership snapshots remain unimplemented; this batch has no hot-path or live-resource effects.
4. The Services UI panel remains a later Phase 6 surface.
5. Task 6.10 smoke must later prove the integrated workspace behavior.

## Commit boundary

The coherent partial commit contains only the owned service model/health/export files, pure contract tests, service fixture, and this report.
