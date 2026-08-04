# Phase 2: Host, IPC, and Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the kernel into one durable per-profile host process and prove that multiple presentation clients can attach, issue idempotent commands, consume realtime state, detach, reconnect, resynchronize, and recover without owning execution.

**Architecture:** `devmanager-host` exclusively owns the writable kernel and accepts current-user clients over an authenticated Windows named pipe. `devmanager-next` is a development-only GPUI shell that starts or attaches to the host. Each connection negotiates capabilities, receives a snapshot plus ordered events, has bounded priority queues, and may detach without stopping the host. Full quit is an explicit host command that drains and reconciles owned resources.

**Tech Stack:** Rust/Tokio, Windows named pipes and security descriptors, MessagePack protocol from Phase 1, SQLite kernel, process-level integration tests.

## Global Constraints

- Exactly one host may own a profile. A stale lock is recovered only after verifying the recorded PID and executable identity are no longer alive.
- The named pipe is scoped to the current Windows user and profile; knowing its name is not sufficient authorization.
- The GPUI client never opens the writable SQLite database and never holds runtime ownership.
- Window close means detach. Full quit is a separate explicit command with a visible dirty/active-resource summary.
- Reconnection uses sequence cursors; any gap or overflow forces a fresh snapshot.
- Control and approval messages outrank bulk terminal/browser updates; every queue is bounded.
- Host recovery reports facts honestly. It may reconcile a resource recipe, but may not claim a prior process survived unless Windows identity checks prove it.
- `devmanager-next` and its profile remain development-only and are deleted/renamed at final cutover.
- Host diagnostics are structured, bounded, and redacted before enqueue. Raw content capture is an explicit time-bounded local diagnostic mode, never a default log level.

---

## File map

- Create: `src/host/mod.rs`
- Create: `src/host/lock.rs`
- Create: `src/host/ipc.rs`
- Create: `src/host/connection.rs`
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
- Modify: `src/lib.rs`
- Modify: `Cargo.toml`
- Create: `src/diagnostics/logging.rs`
- Create: `tests/host_lock.rs`
- Create: `tests/ipc_protocol.rs`
- Create: `tests/host_lifecycle.rs`
- Create: `tests/host_recovery.rs`
- Create: `tests/diagnostic_logging.rs`
- Create: `tests/cli_client.rs`

### Task 2.1: Enforce one host per profile

**Files:** `src/host/{mod,lock}.rs`, `src/bin/devmanager-host.rs`, `Cargo.toml`, `tests/host_lock.rs`

- [ ] **Step 1: Write failing multiprocess tests** `second_host_is_rejected`, `stale_pid_record_is_recovered`, `live_unrelated_pid_is_not_killed`, `different_profiles_can_coexist`, and `lock_records_executable_and_start_time`.
- [ ] **Step 2: Run** `cargo test --test host_lock -- --nocapture` through the Phase 0 wrapper and record the missing binary/module result.
- [ ] **Step 3: Add the `devmanager-host` binary** with explicit `--profile`, `--instance-label`, `--parent-pid` (development launcher only), and `--foreground` arguments. It must reject an empty or production profile in debug builds unless an explicit release invocation satisfies Phase 10.
- [ ] **Step 4: Implement `HostLock`** using an exclusive file handle plus a JSON identity record containing PID, Windows process creation time, canonical executable path, profile, protocol major, and random boot ID.
- [ ] **Step 5: Recover stale metadata only** after comparing PID, creation time, and executable path. Never terminate a process to acquire the lock.
- [ ] **Step 6: Run** `cargo test --test host_lock -- --nocapture`; inspect spawned child cleanup; commit as `feat(host): enforce one host per profile`.

### Task 2.2: Build a current-user authenticated named-pipe server

**Files:** `src/host/ipc.rs`, `src/protocol/{envelope,frame}.rs`, `tests/ipc_protocol.rs`

**Contract:** pipe name is derived from an invariant profile hash and boot-independent product namespace; access is limited to the current user SID and SYSTEM. The first valid frame must be `ClientHello`.

- [ ] **Step 1: Write failing tests** for same-user connection, wrong-profile rejection, pre-hello command rejection, oversized frame closure, malformed frame closure, and five concurrent client handshakes.
- [ ] **Step 2: Run** `cargo test --test ipc_protocol pipe_ -- --nocapture` and save the red output.
- [ ] **Step 3: Create the named pipe** with an explicit security descriptor for current user SID plus SYSTEM, reject remote clients, and use byte mode with protocol framing independent of pipe message boundaries.
- [ ] **Step 4: Add a per-connection nonce** to `ServerHello`; bind the connection to the profile, negotiated major/minor, client ID, and host boot ID.
- [ ] **Step 5: Set read/write/handshake deadlines** and close without decoding additional frames after any protocol violation. Log error category and connection ID but not command payloads.
- [ ] **Step 6: Run** the focused pipe tests and commit as `feat(host): add authenticated named pipe transport`.

### Task 2.3: Implement the reusable host client and capability handshake

**Files:** `src/client/{mod,connection,model}.rs`, `src/protocol/capabilities.rs`, `tests/ipc_protocol.rs`

- [ ] **Step 1: Add failing tests** `compatible_minor_negotiates_intersection`, `incompatible_major_is_visible`, `request_receipt_is_correlated`, `disconnect_fails_pending_requests`, and `client_never_opens_kernel_db`.
- [ ] **Step 2: Run** `cargo test --test ipc_protocol client_ -- --nocapture` and retain the red result.
- [ ] **Step 3: Implement `HostClient::connect`** with profile-derived endpoint, handshake deadline, explicit client build, stable `ClientId`, and requested capability set.
- [ ] **Step 4: Split one connection into a single writer task and reader task**; correlate receipts by `CommandId`, reject duplicate in-flight IDs, and deliver unsolicited server messages to a bounded subscription channel.
- [ ] **Step 5: Expose typed states** `Disconnected`, `Connecting`, `Synchronizing`, `Ready`, `Incompatible`, and `Failed`. UI callers receive state changes rather than transport errors or retry loops.
- [ ] **Step 6: Add a test filesystem observer** proving the client never opens `kernel.sqlite`; run tests and commit as `feat(client): add host connection and negotiation`.

### Task 2.4: Add one action catalog and a scriptable CLI client

**Files:** `src/client/{action,cli}.rs`, `src/bin/devmanager-host.rs`, `src/protocol/capabilities.rs`, `tests/cli_client.rs`

**User-facing entry:** the packaged console-subsystem host binary dispatches `devmanager-host ctl ...` before server bootstrap, so CLI/automation does not add a third shipped binary. Initial commands are `ctl actions --json`, `ctl status --json`, `ctl tasks --json`, `ctl task-show --task-id UUID --json`, and `ctl invoke --action ACTION_ID --arguments-json JSON --expected-task-revision N --json`.

- [ ] **Step 1: Write failing process tests** `ctl_does_not_acquire_host_lock`, `ctl_uses_same_user_pipe`, `actions_are_unique_and_capability_filtered`, `invoke_builds_same_command_as_ui`, `json_output_is_stable`, `nonzero_exit_maps_rejection`, `dangerous_action_requires_current_target_confirmation`, and `automation_reconnects_without_duplicate_command`.
- [ ] **Step 2: Run** `cargo test --test cli_client -- --nocapture` through the isolation wrapper and retain the red missing subcommand/action result.
- [ ] **Step 3: Define `ActionCatalog`** with stable ActionId, title/description, keywords, scope, argument schema, required capability, risk class, availability predicate, and a pure factory from validated arguments plus current client projection to `CommandEnvelope`. Start with host status, Task list/show/create/rename/archive/reopen, and host-quit inspection/confirmation.
- [ ] **Step 4: Dispatch `ctl` before normal host startup** and attach through `HostClient`; emit versioned JSON to stdout, human-readable diagnostics to stderr, and documented exit codes for success, validation, rejection, unavailable host, incompatible protocol, and transport failure.
- [ ] **Step 5: Require dangerous CLI invocations** to include the exact Task/resource target and current expected revision/generation returned by a prior inspection. No `--yes` flag can bypass local host authorization or stale-target checks.
- [ ] **Step 6: Make later phases extend this catalog** when they add provider, terminal, browser, Git, service, Connect, or management commands; GPUI/menus/shortcuts and CLI reference the same ActionIds rather than reimplementing factories.
- [ ] **Step 7: Run** `cargo test --test cli_client -- --nocapture`; commit as `feat(client): add shared actions and host ctl client`.

### Task 2.5: Wire commands, subscriptions, snapshots, and replay

**Files:** `src/host/connection.rs`, `src/client/{model,subscription}.rs`, `src/kernel/command_bus.rs`, `tests/host_lifecycle.rs`

- [ ] **Step 1: Write failing tests** where two clients attach, both see one initial snapshot, client A creates a task, client B receives the ordered event, A retries the command without a duplicate, and B reconnects from its last cursor.
- [ ] **Step 2: Run** `cargo test --test host_lifecycle realtime_ -- --nocapture` and record the red output.
- [ ] **Step 3: Give the host one `CommandBus` executor** and route every `ClientCommand` through it. Transport tasks may never mutate projections directly.
- [ ] **Step 4: On subscribe**, capture a snapshot through sequence N, register the subscriber, then deliver events after N without a race. Persist client cursors only in the client, not as task truth.
- [ ] **Step 5: Use three bounded outbound lanes:** critical receipts/approvals, state events, and bulk deltas. On state overflow send `ResyncRequired`; on bulk overflow coalesce to the newest snapshot marker.
- [ ] **Step 6: Ensure one slow client cannot block command execution or another client.** Add a deterministic slow-reader test with small test-only queue limits.
- [ ] **Step 7: Run** the realtime tests and commit as `feat(host): stream snapshots and ordered events`.

### Task 2.6: Make UI close a detach and full quit explicit

**Files:** `src/host/shutdown.rs`, `src/client/connection.rs`, `src/bin/devmanager-next.rs`, `src/protocol/envelope.rs`, `tests/host_lifecycle.rs`

- [ ] **Step 1: Add failing tests** `client_disconnect_leaves_host_running`, `last_client_detach_leaves_task_open`, `request_quit_returns_active_resource_summary`, `confirmed_quit_drains_then_exits`, and `cancelled_quit_changes_nothing`.
- [ ] **Step 2: Run** `cargo test --test host_lifecycle detach_ -- --nocapture` and save the expected failures.
- [ ] **Step 3: Add commands** `RequestHostQuit` and `ConfirmHostQuit { inspection_id }`; the request returns counts and names of active agents, terminals, browsers, and services plus dirty worktrees without mutating state.
- [ ] **Step 4: Treat EOF, window close, and client crash as detach only.** Remove any client-owned shutdown guard capable of killing host resources.
- [ ] **Step 5: On confirmed quit**, stop accepting mutating commands, drain receipts/outbox state, ask resource supervisors to close, persist final facts, release the lock, and exit with a typed result. Phase 2 uses fake supervisors; Phase 3 supplies real ones.
- [ ] **Step 6: Run** the detach/full-quit tests and commit as `feat(host): separate detach from full quit`.

### Task 2.7: Auto-start and reconnect without duplicate hosts

**Files:** `src/client/connection.rs`, `src/bin/devmanager-next.rs`, `tests/host_lifecycle.rs`

- [ ] **Step 1: Add failing tests** for absent-host startup, ten simultaneous auto-start clients, host-start timeout, reconnect with exponential backoff/jitter, and no auto-restart after an explicit full quit.
- [ ] **Step 2: Run** `cargo test --test host_lifecycle autostart_ -- --nocapture` and retain the red result.
- [ ] **Step 3: Implement attach-first startup.** If the pipe is absent, launch the exact sibling `devmanager-host.exe` with inherited profile/label and a sanitized environment, then race only on the host lock—not on arbitrary sleeps.
- [ ] **Step 4: Retry pipe attachment** with bounded exponential backoff and jitter until the host announces readiness or the startup deadline expires. Concurrent clients may all attempt launch; only the lock winner continues.
- [ ] **Step 5: Reconnect transport failures** while preserving `ClientId` and last applied cursor. Do not resubmit commands whose final receipt is known; safely retry unknown receipts with the same `CommandId`.
- [ ] **Step 6: Run** the focused tests, inspect child-process cleanup, and commit as `feat(client): add safe host autostart and reconnect`.

### Task 2.8: Recover durable state without inventing live resources

**Files:** `src/host/mod.rs`, `src/kernel/runtime.rs`, `src/host/shutdown.rs`, `tests/host_recovery.rs`

- [ ] **Step 1: Write failing crash tests:** kill the host after accepting a command, reopen and return the same receipt; leave stored resource recipes, reopen them as `Recovering`; reconcile a missing process to `StoppedUnexpectedly`; reject a stale generation completion.
- [ ] **Step 2: Run** `cargo test --test host_recovery -- --nocapture` and capture the red result.
- [ ] **Step 3: Bootstrap in order:** acquire lock, open/migrate/integrity-check store, rebuild runtime registry from durable facts, reconcile resources, bind pipe, then publish `Ready`. Clients must not connect to a partially recovered host.
- [ ] **Step 4: Persist shutdown intent and boot ID** so recovery distinguishes clean full quit, process crash, and machine interruption. Never infer that a provider/PTY/browser is alive from a prior `Running` event alone.
- [ ] **Step 5: Make reconciliation idempotent** and generation fenced. Later supervisors implement platform identity checks; Phase 2 fake probes exercise every transition.
- [ ] **Step 6: Run** recovery tests three times to expose ordering races; commit as `feat(host): recover durable state honestly`.

### Task 2.9: Add a bounded host/client update handoff

**Files:** `src/host/update.rs`, `src/protocol/{capabilities,envelope}.rs`, `tests/host_lifecycle.rs`

- [ ] **Step 1: Write failing tests** for same-build attach, compatible rolling attach, incompatible client rejection with required version, host-drain handoff token expiry, and update abort returning the old host to ready state.
- [ ] **Step 2: Run** `cargo test --test host_lifecycle update_ -- --nocapture` and retain the red output.
- [ ] **Step 3: Add a capability-based compatibility window** for one release transition only. Compatibility means shared protocol commands, not a duplicate runtime, file format, or permanent legacy server.
- [ ] **Step 4: Implement `PrepareUpdate`** to stop new resource launches, flush accepted commands, return a short-lived handoff token plus host boot ID, and remain recoverable if the installer/new host never completes.
- [ ] **Step 5: Implement `ResumeAfterAbortedUpdate`** and explicit incompatible build errors. Do not copy live handles between binaries or claim seamless survival of provider/browser processes unless platform tests prove it.
- [ ] **Step 6: Run** update tests and commit as `feat(host): add bounded update handoff`.

### Task 2.10: Add structured, bounded, redacted host diagnostics

**Files:** `src/diagnostics/{mod,logging}.rs`, `src/host/mod.rs`, `src/config/paths.rs`, `tests/diagnostic_logging.rs`

**Contract:** `DiagnosticEvent` carries timestamp, severity, subsystem, stable code, host boot ID, Task/resource/request identities, bounded typed fields, and optional local evidence reference. It never accepts an arbitrary debug dump as a field.

- [ ] **Step 1: Write failing tests** `secret_values_are_redacted_before_queue`, `terminal_and_prompt_content_are_absent_by_default`, `oversized_fields_are_truncated_with_marker`, `rolling_files_respect_count_and_bytes`, `slow_disk_drops_low_priority_not_host_work`, `raw_capture_expires_and_deletes`, and `diagnostics_stay_inside_profile`.
- [ ] **Step 2: Run** `cargo test --test diagnostic_logging -- --nocapture` through the isolation wrapper and retain the red result.
- [ ] **Step 3: Define a field allowlist/redaction pipeline** for command arguments, environment, URLs/query strings, headers/tokens, file paths, provider payloads, terminal/browser content, and errors. Redact synchronously before handing data to the background writer.
- [ ] **Step 4: Implement one bounded background writer** with separate critical/normal queues, size/count rotation, flush-on-fatal/full-quit, and drop counters. Disk stalls cannot block the kernel command bus, PTY reader, browser executor, or UI client.
- [ ] **Step 5: Add explicit local raw-evidence capture** requiring target Task/content classes, expiry, maximum bytes, warning, and deletion action. Store it outside ordinary logs and never upload it automatically.
- [ ] **Step 6: Run** diagnostic tests and inspect seeded secrets across files; commit as `feat(host): add bounded redacted diagnostics`.

### Task 2.11: Prove the multi-process host vertical slice

**Files:** `tests/host_lifecycle.rs`, `scripts/native-next/Invoke-HostSoak.ps1`, `docs/replacement-deletion-ledger.md`

- [ ] **Step 1: Add a test helper** that builds `devmanager-host`, starts it under a temporary profile, attaches two real client processes, exchanges commands/events, kills one client, restarts it, and requests full quit.
- [ ] **Step 2: Run** the test before the soak script exists and record the red missing-script/coverage result.
- [ ] **Step 3: Create `Invoke-HostSoak.ps1`** to repeat attach/detach/reconnect 100 times, randomly interrupt clients, sample host handle/thread/memory counts, and fail on monotonic leaks beyond explicit tolerance.
- [ ] **Step 4: Assert after full quit** that the named pipe is gone, the lock is released, the host PID is dead, the SQLite integrity check passes, and no development child process remains.
- [ ] **Step 5: Update the deletion ledger** with the temporary `devmanager-next` entry and any bridge/re-export introduced so far.
- [ ] **Step 6: Run** the soak plus all Phase 2 tests; commit as `test(host): prove durable multiprocess lifecycle`.

## Phase 2 verification gate

- [ ] Capture production baseline and announce the multiprocess Rust gate.
- [ ] Run `cargo test --test host_lock --test ipc_protocol --test host_lifecycle --test host_recovery --test diagnostic_logging --test cli_client -- --nocapture` through `Invoke-PhaseGate.ps1`.
- [ ] Run `pwsh scripts/native-next/Invoke-HostSoak.ps1 -Iterations 100`.
- [ ] Run `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`.
- [ ] Verify a client process has no open handle to `kernel.sqlite` and killing it does not stop the host.
- [ ] Verify a slow client cannot delay another client's command receipt by more than the defined integration-test bound.
- [ ] Confirm all development PIDs are gone after explicit full quit and no named pipe/lock remains.
- [ ] Compare production `config.json`/`remote.json` hashes and installed PID/start time.
- [ ] Review the complete Phase 2 diff and deletion ledger.

## Phase 2 exit criteria

- One and only one host owns each profile, with safe stale-lock recovery.
- Same-user clients attach through a bounded, versioned named-pipe protocol and never own the writable database.
- `devmanager-host ctl` and GPUI use the same capability-aware ActionIds/command factories; automation gains JSON output without a third product binary.
- Two clients observe the same command receipts, snapshots, and ordered events in realtime.
- UI close, crash, and reconnect do not end task execution; explicit full quit drains and exits.
- Host crash recovery preserves committed facts and receipts while representing live-resource uncertainty honestly.
- Structured diagnostics remain bounded/redacted and cannot backpressure execution or persist raw content by default.
- The host/client soak shows no unbounded handle, memory, connection, or process growth.
