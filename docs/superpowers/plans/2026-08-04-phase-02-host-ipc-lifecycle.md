# Phase 2: Host, IPC, and Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the kernel into one durable per-profile host process and prove that multiple presentation clients can attach, issue idempotent commands, consume realtime state, detach, reconnect, resynchronize, and recover without owning execution.

**Architecture:** `devmanager-host` exclusively owns the writable kernel and accepts current-user clients over an authenticated Windows named pipe. `devmanager-next` is a development-only GPUI shell that starts or attaches to the host. Each connection negotiates capabilities/limits, receives chunked snapshot pages plus ordered durable events and droppable ephemeral streams, has bounded priority queues, and may detach without stopping the host. Full quit first closes admission, then drains and reconciles owned resources through one idempotent lifecycle barrier.

**Tech Stack:** Rust/Tokio, Windows named pipes and security descriptors, MessagePack protocol from Phase 1, SQLite kernel, process-level integration tests.

## Global Constraints

- Exactly one host may own a profile. A stale lock is recovered only after verifying the recorded PID and executable identity are no longer alive.
- The named pipe is scoped to the current Windows user and profile; knowing its name is not sufficient authorization.
- The GPUI client never opens the writable SQLite database and never holds runtime ownership.
- Window close means detach. Full quit is a separate explicit command with a visible dirty/active-resource summary.
- Reconnection uses sequence cursors; any gap or overflow forces a fresh snapshot.
- An accepted receipt resolves command admission only. Clients retain `OperationId` until a durable outcome event or an explicit status query proves success/failure/cancellation/uncertainty.
- Control and approval messages outrank bulk terminal/browser updates; every queue is bounded.
- Initial snapshots are paged/chunked; child transcripts, terminal history, diffs, recordings, and artifacts load incrementally on demand.
- Host recovery reports facts honestly. It may reconcile a resource recipe, but may not claim a prior process survived unless Windows identity checks prove it.
- `devmanager-next` and its profile remain development-only and are deleted/renamed at final cutover.
- Host diagnostics are structured, bounded, and redacted before enqueue. Raw content capture is an explicit time-bounded local diagnostic mode, never a default log level.
- Phase 0 ships only a ValidateOnly isolation scaffold. Complete real `Start-NativeNext.ps1` / `Stop-NativeNext.ps1` lifecycle only after `devmanager-host` and `devmanager-next` binaries plus attach/quit commands exist in this phase. Phase 2 may launch/quit an otherwise process-empty host; Phase 3 is the first zero-orphan acceptance gate (Job Object membership / `ACTIVE_PROCESS_ZERO`). Do not dilute the Rust host authority model with speculative PowerShell process supervision.
- Pure in-process protocol tests with explicit temporary roots may use direct focused Cargo loops. Every test target that can launch a host/client or write profile-backed state uses a predeclared exact recipe from Task 2.0, or a conformance/soak recipe registered before its first run in Task 2.11; the phase exit uses grouped exact recipes. Generic command, argument, or script admission remains forbidden.

---

## File map

- Create: `src/host/mod.rs`
- Create: `src/host/lock.rs`
- Create: `src/host/ipc.rs`
- Create: `src/host/connection.rs`
- Create: `src/host/admission.rs`
- Create: `src/host/shutdown.rs`
- Create: `src/host/update.rs`
- Create: `src/client/mod.rs`
- Create: `src/client/connection.rs`
- Create: `src/client/model.rs`
- Create: `src/client/subscription.rs`
- Create: `src/client/action.rs`
- Create: `src/client/cli.rs`
- Create: `src/bin/devmanager-host.rs`
- Create: `src/bin/devmanager-next.rs`
- Modify: `src/protocol/{capabilities,envelope,frame}.rs`
- Modify: `src/kernel/{command_bus,outbox,runtime}.rs`
- Create: `src/conformance/{mod,manifest,trace,runner,index}.rs`
- Modify: `src/lib.rs`
- Modify: `Cargo.toml`
- Create: `src/diagnostics/logging.rs`
- Create: `tests/host_lock.rs`
- Create: `tests/ipc_protocol.rs`
- Create: `tests/host_lifecycle.rs`
- Create: `tests/host_recovery.rs`
- Create: `tests/host_admission.rs`
- Create: `tests/diagnostic_logging.rs`
- Create: `tests/cli_client.rs`
- Create: `tests/conformance_manifest.rs`
- Create: `tests/conformance_index.rs`
- Create: `tests/host_conformance.rs`
- Create: `tests/support/host_integration.rs`
- Create: `tests/fixtures/conformance/host-v1/{snapshot-replay,lifecycle}.json`
- Modify: `scripts/native-next/{Start-NativeNext,Stop-NativeNext,NativeNext}.ps1`
- Modify: `scripts/native-next/Invoke-PhaseGate.ps1` (inject a contained run directory for registered artifact-producing recipes)
- Create: `scripts/native-next/Invoke-HostSoak.ps1`
- Create: `docs/performance-budgets.md`
- Modify: `scripts/native-next/PhaseGate.ps1` (add exact focused-process, conformance-contract, host-conformance, grouped-test, and clippy recipes)
- Modify: `tests/development_isolation.rs` (lock the Phase 2 recipe vectors and reject generic arguments or script admission)

### Task 2.0: Register exact focused process-test recipes

**Files:** `scripts/native-next/PhaseGate.ps1`, `tests/development_isolation.rs`

- [ ] **Step 1: Write failing recipe-contract tests** for these exact no-argument recipes: `phase-02-host-lock` runs `cargo test --test host_lock -- --nocapture`; `phase-02-cli-client` runs `cargo test --test cli_client -- --nocapture`; `phase-02-host-lifecycle` runs `cargo test --test host_lifecycle -- --nocapture`; `phase-02-host-recovery` runs `cargo test --test host_recovery -- --nocapture`; and `phase-02-diagnostics` runs `cargo test --test diagnostic_logging -- --nocapture`.
- [ ] **Step 2: Add only those vectors** to the closed recipe table. Every recipe keeps the isolated integration-test profile/target policy; none accepts caller arguments or script paths.
- [ ] **Step 3: Run** the Phase Gate contract tests only, confirm production invariants and zero process residue, then commit as `test(host): register guarded focused recipes`.

### Task 2.1: Enforce one host per profile

**Files:** `src/host/{mod,lock}.rs`, `src/bin/devmanager-host.rs`, `Cargo.toml`, `tests/host_lock.rs`

- [ ] **Step 1: Write failing multiprocess tests** `second_host_is_rejected`, `stale_pid_record_is_recovered`, `live_unrelated_pid_is_not_killed`, `different_profiles_can_coexist`, and `lock_records_executable_and_start_time`.
- [ ] **Step 2: Run** `pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-02-host-lock-red -Recipe phase-02-host-lock -LongRustRun` and record the missing binary/module result.
- [ ] **Step 3: Add the `devmanager-host` binary** with explicit `--profile`, `--instance-label`, `--parent-pid` (development launcher only), and `--foreground` arguments. It must reject an empty or production profile in debug builds unless an explicit release invocation satisfies Phase 11.
- [ ] **Step 4: Implement `HostLock`** using an exclusive file handle plus a JSON identity record containing PID, Windows process creation time, canonical executable path, profile, protocol major, and random boot ID.
- [ ] **Step 5: Recover stale metadata only** after comparing PID, creation time, and executable path. Never terminate a process to acquire the lock.
- [ ] **Step 6: Run** the same `phase-02-host-lock` recipe green; inspect spawned child cleanup; commit as `feat(host): enforce one host per profile`.

### Task 2.2: Build a current-user authenticated named-pipe server

**Files:** `src/host/ipc.rs`, `src/protocol/{envelope,frame}.rs`, `tests/ipc_protocol.rs`

**Contract:** pipe name is derived from an invariant profile hash and boot-independent product namespace; access is limited to the current user SID and SYSTEM. The first valid frame must be `ClientHello`. Every direct `ipc_protocol` test constructs a process-unique temporary root and named profile before binding; no fixture may resolve the normal app-data root or production pipe namespace.

- [ ] **Step 1: Write failing tests** for same-user connection, wrong-profile rejection, pre-hello command rejection, negotiated lower frame/page/chunk limits, oversized four-byte header rejection before allocation, malformed frame closure, unknown optional extension tolerance, and five concurrent client handshakes.
- [ ] **Step 2: Run** `cargo test --test ipc_protocol pipe_ -- --nocapture` and save the red output.
- [ ] **Step 3: Create the named pipe** with an explicit security descriptor for current user SID plus SYSTEM, reject remote clients, and use byte mode with protocol framing independent of pipe message boundaries.
- [ ] **Step 4: Add a per-connection nonce and negotiated `FrameLimits`** to `ServerHello`; bind the connection to the profile, negotiated major/minor, client ID, host boot ID, and per-field minimum limits.
- [ ] **Step 5: Set read/write/handshake deadlines** and close without decoding additional frames after any protocol violation. Log error category and connection ID but not command payloads.
- [ ] **Step 6: Run** the focused pipe tests and commit as `feat(host): add authenticated named pipe transport`.

### Task 2.3: Implement the reusable host client and capability handshake

**Files:** `src/client/{mod,connection,model}.rs`, `src/protocol/capabilities.rs`, `tests/ipc_protocol.rs`

**Test isolation:** reuse the process-unique temporary root/profile fixture from Task 2.2 for every direct client test, and assert the client never resolves production storage or the production pipe namespace.

- [ ] **Step 1: Add failing tests** `compatible_minor_negotiates_intersection`, `incompatible_major_is_visible`, `request_receipt_is_correlated`, `accepted_receipt_keeps_operation_pending`, `operation_settlement_survives_reconnect`, `disconnect_preserves_unknown_operation_for_status_query`, and `client_never_opens_kernel_db`.
- [ ] **Step 2: Run** `cargo test --test ipc_protocol client_ -- --nocapture` and retain the red result.
- [ ] **Step 3: Implement `HostClient::connect`** with profile-derived endpoint, handshake deadline, explicit client build, stable `ClientId`, and requested capability set.
- [ ] **Step 4: Split one connection into a single writer task and reader task**; correlate mutation receipts by `CommandId`, query replies by `RequestId`, track accepted work by `OperationId`, reject duplicate in-flight IDs, and deliver outcome/unsolicited server messages to a bounded subscription channel. Transport loss fails only the query/receipt wait; it does not relabel an accepted operation failed, settled, cancelled, or uncertain.
- [ ] **Step 5: Expose typed states** `Disconnected`, `Connecting`, `Synchronizing`, `Ready`, `Incompatible`, and `Failed`. UI callers receive state changes rather than transport errors or retry loops.
- [ ] **Step 6: Add a test filesystem observer** proving the client never opens `kernel.sqlite`; run tests and commit as `feat(client): add host connection and negotiation`.

### Task 2.4: Add one action catalog and a scriptable CLI client

**Files:** `src/client/{action,cli}.rs`, `src/bin/devmanager-host.rs`, `src/protocol/capabilities.rs`, `tests/cli_client.rs`

**User-facing entry:** the packaged console-subsystem host binary dispatches `devmanager-host ctl ...` before server bootstrap, so CLI/automation does not add a third shipped binary. Initial commands are `ctl actions --json`, `ctl status --json`, `ctl tasks --json`, `ctl task-show --task-id UUID --json`, and `ctl invoke --action ACTION_ID --arguments-json JSON --expected-task-revision N --json`.

- [ ] **Step 1: Write failing process tests** `ctl_does_not_acquire_host_lock`, `ctl_uses_same_user_pipe`, `actions_are_unique_and_capability_filtered`, `invoke_builds_same_command_as_ui`, `json_output_is_stable`, `nonzero_exit_maps_rejection`, `dangerous_action_requires_current_target_confirmation`, and `automation_reconnects_without_duplicate_command`.
- [ ] **Step 2: Run** the `phase-02-cli-client` recipe and retain the red missing subcommand/action result.
- [ ] **Step 3: Define `ActionCatalog`** with stable ActionId, title/description, keywords, scope, argument schema, required capability, risk class, availability predicate, and a pure factory from validated arguments plus current client projection to `ClientRequest::{Query(QueryEnvelope), Mutation(CommandEnvelope)}`. Start with host status, Task list/show/create/rename/archive/reopen, and host-quit inspection/confirmation; read-only actions never allocate operations.
- [ ] **Step 4: Dispatch `ctl` before normal host startup** and attach through `HostClient`; emit versioned JSON to stdout, human-readable diagnostics to stderr, and documented exit codes for success, validation, rejection, unavailable host, incompatible protocol, and transport failure.
- [ ] **Step 5: Require dangerous CLI invocations** to include the exact Task/resource target and current expected revision/generation returned by a prior inspection. No `--yes` flag can bypass local host authorization or stale-target checks.
- [ ] **Step 6: Make later phases extend this catalog** when they add provider, terminal, browser, Git, service, Connect, or management commands; GPUI/menus/shortcuts and CLI reference the same ActionIds rather than reimplementing factories.
- [ ] **Step 7: Run** the same `phase-02-cli-client` recipe green; commit as `feat(client): add shared actions and host ctl client`.

### Task 2.5: Wire commands, layered subscriptions, paged snapshots, and replay

**Files:** `src/host/connection.rs`, `src/client/{model,subscription}.rs`, `src/kernel/command_bus.rs`, `tests/host_lifecycle.rs`

- [ ] **Step 1: Write failing tests** where two clients attach, both assemble the same multi-page initial snapshot, client A creates a task, client B receives the ordered event, A retries the command without a duplicate, an ephemeral stream frame is coalesced under pressure, a large child transcript is omitted until requested, and B reconnects from its last cursor. Add corrupted/expired snapshot cursor, interrupted chunk resume, slow bulk reader, and operation-settlement-after-reconnect cases.
- [ ] **Step 2: Run** the `phase-02-host-lifecycle` recipe and record the red realtime failures.
- [ ] **Step 3: Give the host one `CommandBus` executor** and route every mutation `CommandEnvelope` through it; route side-effect-free `QueryEnvelope`s through the read boundary. Transport tasks may never mutate projections directly.
- [ ] **Step 4: On subscribe**, freeze a snapshot through sequence N, register the subscriber, deliver bounded pages under the negotiated item/byte limits, then deliver durable events after N without a race. Snapshot/chunk cursors are opaque and resumable; persist applied cursors only in the client, not as task truth.
- [ ] **Step 5: Use three explicit bounded server-to-client layers:** critical receipts/query replies/settlements/interactive requests, durable ordered state events, and ephemeral resource streams. Critical traffic may disconnect a non-reading client but is never silently dropped; state overflow sends `ResyncRequired`; ephemeral progress/status may coalesce to the newest generation/sequence marker.
- [ ] **Step 6: Add on-demand subscriptions** for child transcripts, terminal scrollback, diffs, recordings, and artifacts. Each starts from a typed resource cursor and negotiated page/chunk limit; none is embedded wholesale in the Task snapshot.
- [ ] **Step 7: Ensure one slow client cannot block command execution or another client.** Add a deterministic slow-reader test with small test-only queue limits.
- [ ] **Step 8: Preserve the deterministic snapshot/replay/backpressure assertions and declared metrics** for Tasks 2.13–2.14, including page/chunk counts, dropped/coalesced ephemeral frames, forced resyncs, acknowledgement latency, and settlement convergence. Do not emit canonical conformance artifacts before Task 2.11 establishes their format.
- [ ] **Step 9: Run** the same `phase-02-host-lifecycle` recipe green and commit as `feat(host): stream paged snapshots durable events and ephemeral state`.

### Task 2.6: Make detach explicit and close through one admission barrier

**Files:** `src/host/{admission,shutdown}.rs`, `src/client/connection.rs`, `src/bin/devmanager-next.rs`, `src/protocol/envelope.rs`, `tests/{host_admission,host_lifecycle}.rs`

- [ ] **Step 1: Add failing tests** `client_disconnect_leaves_host_running`, `last_client_detach_leaves_task_open`, `begin_close_rejects_new_side_effects_before_drain`, `accepted_pre_close_effect_settles_or_cancels`, `duplicate_close_returns_same_operation`, `request_quit_returns_active_resource_summary`, `confirmed_quit_drains_then_exits`, `cleanup_failure_prevents_closed_claim`, and `cancelled_quit_changes_nothing`.
- [ ] **Step 2: Run** the `phase-02-host-lifecycle` recipe and save the expected detach failures.
- [ ] **Step 3: Define `AdmissionState::{Open, Closing { operation_id, action_epoch }, Closed}`** for Task and host scope. `BeginCloseTask`/confirmed host quit atomically enter `Closing`, advance the action epoch, publish it, and reject every later provider send/launch, terminal/browser/service command, background-job registration, and side-effect retry with `Closing` before teardown begins.
- [ ] **Step 4: Add side-effect-free query** `InspectHostQuit` and mutation command `ConfirmHostQuit { inspection_id }`; inspection returns counts and names of active agents, terminals, browsers, and services plus dirty worktrees without allocating an operation or mutating state. Confirmation returns `Accepted { operation_id }`; it does not report success until the shutdown operation settles.
- [ ] **Step 5: Treat EOF, window close, and client crash as detach only.** Remove any client-owned shutdown guard capable of killing host resources.
- [ ] **Step 6: On confirmed quit**, cancel or drain previously accepted fake supervisor work under bounded deadlines, persist an explicit result for each branch, flush final durable events/outbox, and settle the quit operation before releasing the lock. A failed branch yields `CleanupFailed` and residue evidence; it never publishes `Closed`. Phase 3 replaces fake supervisors with Job/PTy ownership while keeping this barrier.
- [ ] **Step 7: Make close/dispose idempotent** by caching the scope's operation ID/state. Concurrent Task close/full quit share the existing operation and cannot launch duplicate cleanup futures.
- [ ] **Step 8: Run** the same `phase-02-host-lifecycle` recipe green and commit as `feat(host): gate close with durable admission barrier`.

### Task 2.7: Auto-start and reconnect without duplicate hosts

**Files:** `src/client/connection.rs`, `src/bin/devmanager-next.rs`, `tests/host_lifecycle.rs`

- [ ] **Step 1: Add failing tests** for absent-host startup, ten simultaneous auto-start clients, host-start timeout, reconnect with exponential backoff/jitter, and no auto-restart after an explicit full quit.
- [ ] **Step 2: Run** the `phase-02-host-lifecycle` recipe and retain the red autostart result.
- [ ] **Step 3: Implement attach-first startup.** If the pipe is absent, launch the exact sibling `devmanager-host.exe` with inherited profile/label and a sanitized environment, then race only on the host lock—not on arbitrary sleeps.
- [ ] **Step 4: Retry pipe attachment** with bounded exponential backoff and jitter until the host announces readiness or the startup deadline expires. Concurrent clients may all attempt launch; only the lock winner continues.
- [ ] **Step 5: Reconnect transport failures** while preserving `ClientId` and last applied cursor. Do not resubmit commands whose final receipt is known; safely retry unknown receipts with the same `CommandId`.
- [ ] **Step 6: Run** the same `phase-02-host-lifecycle` recipe green, inspect child-process cleanup, and commit as `feat(client): add safe host autostart and reconnect`.

### Task 2.8: Recover durable state without inventing live resources

**Files:** `src/host/mod.rs`, `src/kernel/runtime.rs`, `src/host/shutdown.rs`, `tests/host_recovery.rs`

- [ ] **Step 1: Write failing crash tests:** kill the host after accepting but before settling a command, reopen and return the same receipt/operation state; finish an outbox effect then crash before outcome recording and reconcile without duplicate effect; when proof is unavailable, transition a non-retry-safe effect to `Uncertain` without redispatch; leave stored resource recipes, reopen them as `Recovering`; preserve `Closing` admission and reject new work after restart; reconcile a missing process to `StoppedUnexpectedly`; reject a stale generation/action-epoch completion.
- [ ] **Step 2: Run** the `phase-02-host-recovery` recipe and capture the red result.
- [ ] **Step 3: Bootstrap in order:** acquire lock, open/migrate/integrity-check store, rebuild runtime registry from durable facts, reconcile resources, bind pipe, then publish `Ready`. Clients must not connect to a partially recovered host.
- [ ] **Step 4: Persist shutdown intent, admission state, accepted operation state, and boot ID** so recovery distinguishes clean full quit, in-progress close, process crash, and machine interruption. Never infer that a provider/PTY/browser is alive from a prior `Running` event alone.
- [ ] **Step 5: Make reconciliation idempotent** and generation fenced. Later supervisors implement platform identity checks; Phase 2 fake probes exercise every transition.
- [ ] **Step 6: Run** the `phase-02-host-recovery` recipe three times to expose ordering races; commit as `feat(host): recover durable state honestly`.

### Task 2.9: Add a bounded host/client update handoff

**Files:** `src/host/update.rs`, `src/protocol/{capabilities,envelope}.rs`, `tests/host_lifecycle.rs`

- [ ] **Step 1: Write failing tests** for same-build attach, compatible rolling attach, incompatible client rejection with required version, host-drain handoff token expiry, and update abort returning the old host to ready state.
- [ ] **Step 2: Run** the `phase-02-host-lifecycle` recipe and retain the red update-handoff failures.
- [ ] **Step 3: Add a capability-based compatibility window** for one release transition only. Compatibility means shared protocol commands, not a duplicate runtime, file format, or permanent legacy server.
- [ ] **Step 4: Implement `PrepareUpdate`** to stop new resource launches, flush accepted commands, return a short-lived handoff token plus host boot ID, and remain recoverable if the installer/new host never completes.
- [ ] **Step 5: Implement `ResumeAfterAbortedUpdate`** and explicit incompatible build errors. Do not copy live handles between binaries or claim seamless survival of provider/browser processes unless platform tests prove it.
- [ ] **Step 6: Run** the same `phase-02-host-lifecycle` recipe green and commit as `feat(host): add bounded update handoff`.

### Task 2.10: Add structured, bounded, redacted host diagnostics

**Files:** `src/diagnostics/{mod,logging}.rs`, `src/host/mod.rs`, `src/config/paths.rs`, `tests/diagnostic_logging.rs`

**Contract:** `DiagnosticEvent` carries timestamp, severity, subsystem, stable code, host boot ID, Task/resource/request identities, bounded typed fields, and optional local evidence reference. It never accepts an arbitrary debug dump as a field.

- [ ] **Step 1: Write failing tests** `secret_values_are_redacted_before_queue`, `terminal_and_prompt_content_are_absent_by_default`, `oversized_fields_are_truncated_with_marker`, `rolling_files_respect_count_and_bytes`, `slow_disk_drops_low_priority_not_host_work`, `raw_capture_expires_and_deletes`, and `diagnostics_stay_inside_profile`.
- [ ] **Step 2: Run** the `phase-02-diagnostics` recipe and retain the red result.
- [ ] **Step 3: Define a field allowlist/redaction pipeline** for command arguments, environment, URLs/query strings, headers/tokens, file paths, provider payloads, terminal/browser content, and errors. Redact synchronously before handing data to the background writer.
- [ ] **Step 4: Implement one bounded background writer** with separate critical/normal queues, size/count rotation, flush-on-fatal/full-quit, and drop counters. Disk stalls cannot block the kernel command bus, PTY reader, browser executor, or UI client.
- [ ] **Step 5: Add explicit local raw-evidence capture** requiring target Task/content classes, expiry, maximum bytes, warning, and deletion action. Store it outside ordinary logs and never upload it automatically.
- [ ] **Step 6: Run** the same `phase-02-diagnostics` recipe green and inspect seeded secrets across files; commit as `feat(host): add bounded redacted diagnostics`.

### Task 2.11: Establish the canonical artifact contract and rebuildable index

**Files:** `src/conformance/{mod,manifest,trace,runner,index}.rs`, `tests/{conformance_manifest,conformance_index,development_isolation}.rs`, `scripts/native-next/{PhaseGate,Invoke-PhaseGate}.ps1`

- [ ] **Step 1: Write failing contract tests** for versioned immutable `manifest.json`, bounded append-only MessagePack `trace.dmtrace` with hash chaining, resumable `cursor.json`, terminal immutable `result.json`, and rejection of raw prompts/responses, credentials, and absolute user paths. Require resume to validate the manifest, trace chain, and cursor; a completed run never resumes.
- [ ] **Step 2: Register closed no-argument recipes before their first run** and lock every vector in `tests/development_isolation.rs`: `phase-02-conformance-contracts` runs `cargo test --test conformance_manifest --test conformance_index -- --nocapture`; `phase-02-host-conformance` runs `cargo test --test host_conformance -- --nocapture`; `phase-02-host-soak` runs the fixed repository-owned `pwsh -NoProfile -File scripts/native-next/Invoke-HostSoak.ps1` with no caller arguments; `phase-02-tests` runs `cargo test --test host_lock --test ipc_protocol --test host_admission --test host_lifecycle --test host_recovery --test diagnostic_logging --test cli_client --test conformance_manifest --test conformance_index --test host_conformance -- --nocapture`; and `phase-02-clippy` runs `cargo clippy --all-targets -- -D warnings`. Reuse `cargo-fmt-check`; never reintroduce generic command, argument, or script-path admission. For every artifact-producing recipe, including the soak, `Invoke-PhaseGate.ps1` creates and injects a contained, process-unique run-directory environment itself; callers cannot provide an artifact path, run ID, case, arm, or extra arguments.
- [ ] **Step 3: Run** `phase-02-conformance-contracts` through the Phase Gate, confirm its injected directory is beneath the ignored Phase 2 evidence root and production/process invariants hold, and retain the red contract result.
- [ ] **Step 4: Implement the artifact contract and rebuildable index.** Use typed case/arm/run IDs. The local SQLite mirror is derived only from completed, verified manifests/results/traces; it may be deleted or corrupted and rebuilt, never writes canonical evidence, and never copies raw trace payloads. Fixtures declare every scalar metric ID, unit, and aggregation; undeclared metrics are rejected.
- [ ] **Step 5: Run** the same `phase-02-conformance-contracts` recipe green; commit as `feat(conformance): establish immutable artifact contract`.

### Task 2.12: Add real development lifecycle scripts and the host integration helper

**Files:** `scripts/native-next/{Start-NativeNext,Stop-NativeNext,NativeNext}.ps1`, `tests/{host_lifecycle,development_isolation}.rs`, `tests/support/host_integration.rs`

- [ ] **Step 1: Write failing lifecycle tests** for a helper that builds `devmanager-host`, starts it under a process-unique temporary profile, attaches two real client processes, exchanges a command and events, kills and restarts one client, and requests an explicit full quit. The helper rejects production roots and pipe namespaces.
- [ ] **Step 2: Run** the existing exact `phase-02-host-lifecycle` recipe and retain the red lifecycle/helper result.
- [ ] **Step 3: Replace the Phase 0 ValidateOnly scaffold** with real `Start-NativeNext.ps1` / `Stop-NativeNext.ps1` lifecycle only now that `devmanager-host` and `devmanager-next` binaries plus attach/quit commands exist. Keep Capture/Assert guards. Scripts launch through the Rust host authority only and do not gain PID/name ownership logic; Phase 3 retains Job Object and zero-orphan acceptance authority.
- [ ] **Step 4: Implement the contained host integration helper** used by later conformance tests. Its temporary profile, clients, and host all remain inside the Phase Gate-provided environment; it has no caller-selected storage or evidence paths.
- [ ] **Step 5: Run** the same `phase-02-host-lifecycle` recipe green, inspect full-quit cleanup, and commit as `feat(host): add real development lifecycle`.

### Task 2.13: Record fixed real-host conformance cases

**Files:** `tests/host_conformance.rs`, `tests/support/host_integration.rs`, `tests/fixtures/conformance/host-v1/{snapshot-replay,lifecycle}.json`

- [ ] **Step 1: Add committed, fixed fixtures and failing real-host cases** for initial snapshot, durable replay, ephemeral coalescing, forced resync, operation settlement after reconnect, and the admission-close race. The test has no caller-selected case or arm surface.
- [ ] **Step 2: Run** `pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-02-host-conformance-red -Recipe phase-02-host-conformance -LongRustRun` and retain the red result from the Gate-created run directory.
- [ ] **Step 3: Implement the fixed `host_conformance` integration test** using the Task 2.12 helper. For every committed fixture it writes both Baseline and Variant artifacts to the one directory injected by the Phase Gate, records the fixture identity and declared measurements, and cannot write outside that directory or select arbitrary cases/arms at invocation time.
- [ ] **Step 4: Assert after each full quit** that the named pipe is gone, the lock is released, the host PID is dead, the SQLite integrity check passes, and no development child process remains. Phase 2 proves launch/quit of an otherwise process-empty host; Phase 3 remains the residual-orphan acceptance gate.
- [ ] **Step 5: Run** the same `phase-02-host-conformance` recipe green, rebuild the index from its fixed artifacts, and commit as `test(host): record fixed lifecycle conformance`.

### Task 2.14: Soak the vertical slice and record final performance evidence

**Files:** `scripts/native-next/Invoke-HostSoak.ps1`, `tests/host_conformance.rs`, `docs/{performance-budgets,replacement-deletion-ledger}.md`

- [ ] **Step 1: Write failing soak assertions** that repeat attach/detach/reconnect 100 times, randomly interrupt clients, and fail on monotonic leaks beyond explicit tolerance. Record only cold/warm host startup, client attach, reconnect, host handles, host threads, and host memory; PTY, paint, browser, remote, and provider metrics remain with their later owning phases.
- [ ] **Step 2: Define the measurement methods, sample sizes, and Phase 2 bounds** in `docs/performance-budgets.md`, then implement the fixed `Invoke-HostSoak.ps1` loop with no parameters and no caller-selected profile, evidence path, case, arm, or provider authentication. It refuses to run unless the Phase Gate supplied its process-unique contained run directory.
- [ ] **Step 3: Run** `pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-02-host-soak -Recipe phase-02-host-soak -LongRustRun`, then run the `phase-02-host-conformance` recipe; retain the isolated evidence, rebuild the index, and compare both fixed Baseline/Variant arms against the declared budgets.
- [ ] **Step 4: Update the deletion ledger** with the temporary `devmanager-next` entry and every bridge/re-export introduced so far.
- [ ] **Step 5: Run** the phase's grouped test recipe, guarded `phase-02-host-soak` recipe, and focused host conformance proof green; commit as `test(host): prove durable multiprocess lifecycle`.

## Phase 2 verification gate

- [ ] Capture production baseline and announce the multiprocess Rust gate.
- [ ] Run `pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-02-tests -Recipe phase-02-tests -LongRustRun`.
- [ ] Run `pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-02-host-soak -Recipe phase-02-host-soak -LongRustRun`; never launch `Invoke-HostSoak.ps1` directly.
- [ ] Run `pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-02-fmt -Recipe cargo-fmt-check` and `pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-02-clippy -Recipe phase-02-clippy -LongRustRun`.
- [ ] Run `pwsh -NoProfile -File scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-02-host-conformance -Recipe phase-02-host-conformance -LongRustRun`.
- [ ] Verify a client process has no open handle to `kernel.sqlite` and killing it does not stop the host.
- [ ] Verify a slow client cannot delay another client's command receipt by more than the defined integration-test bound.
- [ ] Verify paged snapshots/chunks resume under interruption, unknown optional extensions degrade safely, and no accepted operation is displayed settled before its event/status proves it.
- [ ] Rebuild the conformance query index and compare snapshot/replay/admission baseline and variant arms.
- [ ] Compare the declared isolated host startup/attach/reconnect/handle/thread/memory evidence against the Phase 2 budgets; later-surface metrics remain unavailable until their owner phase.
- [ ] Confirm all development PIDs are gone after explicit full quit and no named pipe/lock remains.
- [ ] Compare production `config.json`/`remote.json` hashes and installed PID/start time.
- [ ] Review the complete Phase 2 diff and deletion ledger.

## Phase 2 exit criteria

- One and only one host owns each profile, with safe stale-lock recovery.
- Same-user clients attach through a bounded, versioned named-pipe protocol and never own the writable database.
- `devmanager-host ctl` and GPUI use the same capability-aware ActionIds/command factories; automation gains JSON output without a third product binary.
- Two clients converge through paged initial state, ordered durable events, coalescible ephemeral streams, and on-demand resource history in realtime.
- Accepted operations remain pending until a correlated outcome, survive reconnect/restart, and are queryable by OperationId; uncertain external delivery remains visible and is never silently retried.
- UI close, crash, and reconnect do not end task execution; Task/full-host close rejects new work first, is idempotent, and cannot claim success while cleanup residue remains.
- Host crash recovery preserves committed facts and receipts while representing live-resource uncertainty honestly.
- Structured diagnostics remain bounded/redacted and cannot backpressure execution or persist raw content by default.
- Canonical conformance artifacts resume safely, completed runs remain immutable, and the local index rebuilds without raw payload retention.
- The host/client soak shows no unbounded handle, memory, connection, or process growth.
