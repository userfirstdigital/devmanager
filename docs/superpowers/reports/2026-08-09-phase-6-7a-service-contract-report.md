# Phase 6 Task 6.7a: Service Contract Report

Status: PARTIAL 6.7a BatchA complete. This review batch hardens the dependency-safe configured-service model contract only. It emits validated plans and redacted evidence; it does not launch processes, probe live ports, modify health behavior, or touch ConfigStore, terminal ownership, UI, or installed-app state.

## RED / GREEN

- RED: the resumed focused run first failed on the intentionally missing per-member plan fence and atomic revalidation contract (`ordered[*].fence`, `StartPlan::revalidate`). Follow-up RED tests also caught direct nested serde accepting unknown/invalid values and command Debug exposing raw argument values, plus non-canonical cwd forms being accepted.
- GREEN: the final focused run passed 19/19 tests, covering validated exact wire deserialization, structural secret rejection and redacted Debug/serialization, canonical workspace-relative cwd paths, port mismatch/derivation, deterministic dependency/cycle handling, fake-clock health behavior, reducer states, exact task/host ownership, dependency/member-operation refusal, closing-task barriers, per-member fences, atomic stale-plan revalidation, duplicate start coalescing, external-port refusal, dependent failure refusal, manual stop, and task-close ownership.
- Formatting: `cargo fmt --all -- --check` passed for the owned Rust changes.
- Existing unrelated warnings remain in `src/kernel/mod.rs` and `src/host/connection.rs`; no service-contract warning was introduced.

## Delivered contract

- `ServiceId`, `ServiceDefinition`, and `LaunchIntent` use private wire types and validated deserializers; bounded command/cwd/env-reference/port/policy values, task/host scope, and explicit validation errors reject empty/duplicate/self/unknown dependencies, cycles, unsafe paths, raw secret-shaped values, and count/text overflow.
- Every public nested contract type uses a validating private wire representation with exact required fields and `deny_unknown_fields`; invalid direct serde values cannot bypass leaf validation. There is no unversioned persistence writer in this slice; ConfigStore remains the later owner of any versioned service-config envelope.
- Command arguments reject secret options both as separate flag/value pairs and inline `=` forms; environment entries are name-only references and reject assignments. Raw secret values never enter the validated launch contract.
- Command Debug output redacts every argv value, invalid command serialization fails closed, and cwd is restricted to canonical workspace-relative paths with the service scope as its task/host authority reference; no shell command string is introduced.
- Health and expected port have one source of truth: a matching explicit expected port is accepted, while a missing expected port is derived from the health port.
- `ServiceCatalog` produces deterministic dependency-first launch plans and dependent-first stop plans. It never creates an effect for an external observation.
- `HealthTracker` consumes scheduled fake-clock observations only, enforcing startup deadlines, probe intervals, bounded exponential backoff, consecutive success/failure thresholds, stale evidence, cancellation, process exit, and generation fences.
- `reduce_service` keeps lifecycle, process, health, port, and ownership axes separate. External occupied ports project blue `External` and admission rejects Stop/Restart/Start control for them. Running without fresh health evidence does not project `Healthy`.
- Typed admission decisions cover Start, Stop, Restart, coalesced duplicate operations, exact task/host ownership, dependency readiness/order, member-operation refusal, and task-close plans that require the `closing_tasks` barrier and exact task epoch.
- Every start/dependency, stop/restart, and task-close plan member carries its exact service generation/epoch/ownership fence. Pure plan revalidation checks the same snapshot for every member and fails closed on fence, state, ownership, closing-barrier, or operation changes.
- `RedactedServiceSnapshot` and `RedactedDiagnostic` expose bounded timestamps, evidence kinds, and provenance without command lines, environment values, or arbitrary output.

## Later blockers / out of scope

1. ConfigStore resolution and environment layering must later produce these validated definitions without materializing secrets into this contract.
2. Phase 3 Job-owned `TerminalService` supervisor/effects must consume plans and attach real process ownership.
3. Health review BatchB is explicitly deferred: real background health probes and port/process ownership snapshots remain unimplemented; this batch has no health-file or live-resource effects.
4. The Services UI panel remains a later Phase 6 surface.
5. Task 6.10 smoke must later prove the integrated workspace behavior.

## Commit boundary

The coherent partial commit contains only the owned service model, model-focused contract tests, valid service fixture, and this report. Health BatchB remains a separate explicit change.
