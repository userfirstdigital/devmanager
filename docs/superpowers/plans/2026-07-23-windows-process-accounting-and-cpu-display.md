# Windows Process Accounting and CPU Display Implementation Plan

> **Execution constraints:** Follow test-driven development. Do not edit CommandIT or external tool configuration. Do not launch or mutate the installed DevManager or its production remote state during verification.

**Goal:** Make DevManager report and control the complete verified process set for each terminal and present CPU with the same whole-machine convention as Windows Task Manager.

**Architecture:** Resource sampling unions live ancestry, Windows Job Object membership, and verified ledger identities into one owned PID set. Runtime CPU fields carry normalized whole-machine percentages plus the logical CPU count; equivalent cores are derived in presentation. Process names are reduced through a safe allowlisted classifier.

**Tech stack:** Rust 2021, GPUI 0.2.2, `sysinfo` 0.38.4, Win32 Job Objects.

---

### Task 1: Lock the resource contract with red tests

**Files:**

- Modify: `src/state/runtime_state.rs`
- Modify: `src/services/process_manager.rs`
- Modify: `src/services/pid_file.rs`
- Modify: `src/app/process_monitor.rs`

- [x] Add pure tests for `125%` core-scaled usage on 64 logical CPUs producing `1.953125%` system usage and `1.25` equivalent cores.
- [x] Add edge tests for zero CPUs, negative/non-finite input, and aggregate clamping.
- [x] Add a PID-selection regression where a worker has lost its parent link but remains a Job member.
- [x] Add a verified-ledger regression where a still-running detached worker survives a descendant sync.
- [x] Add classifier tests for Vitest, Context7, npm, npx, generic Node, and secret-bearing unknown arguments.
- [x] Run the focused filters and observe failures caused by missing production behavior.

### Task 2: Query Windows Job Object membership

**Files:**

- Modify: `src/services/platform_service.rs`
- Modify: `src/terminal/session.rs`

- [x] Bind `QueryInformationJobObject`.
- [x] Add an aligned, dynamically growing parser for `JobObjectBasicProcessIdList`.
- [x] Expose `ManagedProcessJob::active_process_ids`.
- [x] Expose a narrow `TerminalSession` method returning optional managed PIDs.
- [x] Preserve non-Windows compilation with a no-Job result.
- [x] Stop discarding Job attach errors; log a concise diagnostic while preserving launch fallback.
- [x] Add a Windows integration test that assigns a process, spawns a child, and observes both PIDs.
- [x] Run platform-service and terminal-session tests.

### Task 3: Unify process discovery and ledger retention

**Files:**

- Modify: `src/services/process_manager.rs`
- Modify: `src/services/pid_file.rs`

- [x] Snapshot live `TerminalSession` references without retaining the sessions lock.
- [x] Union the verified root, ancestry, managed Job members, and identity-matched ledger descendants.
- [x] Refresh command metadata for sampled processes without persisting raw commands.
- [x] Build memory, CPU, process count, IDs, labels, and parent metadata from that same PID set.
- [x] Sync the complete verified descendant set back to the ledger.
- [x] Make pre-respawn and root-release syncs retain verified known descendants.
- [x] Ensure kill authorization and reap collection accept the same verified owned PIDs.
- [x] Run process-manager and PID-ledger tests.

### Task 4: Normalize CPU and improve monitor labels

**Files:**

- Modify: `src/state/runtime_state.rs`
- Modify: `src/services/process_manager.rs`
- Modify: `src/app/process_monitor.rs`
- Modify: `src/terminal/view.rs`
- Modify: `src/sidebar/mod.rs`

- [x] Add `logical_cpu_count` to `ResourceSnapshot` with a backward-compatible serde default.
- [x] Normalize every process CPU value before storing it.
- [x] Normalize and clamp session totals.
- [x] Show compact whole-machine CPU in terminal headers and the sidebar.
- [x] Show `N.N% system · N.NN cores` in the expanded process monitor.
- [x] Use the safe role classifier for known Node subprocesses.
- [x] Run state and process-monitor tests.

### Task 5: Review and verify

- [x] Run `cargo fmt -- --check`.
- [x] Run focused tests for `platform_service`, `pid_file`, `process_manager`, `runtime_state`, and `process_monitor`.
- [x] Run the complete serial library test suite.
- [x] Run `cargo clippy --all-targets --all-features` (exit 0 with the repository's existing warning baseline).
- [x] Run `cargo build --all-features`.
- [x] Exercise a Windows process harness whose intermediate launcher exits while its worker remains alive; confirm the worker stays in the session PID set and its memory is counted.
- [x] Review the entire diff for raw command leakage, PID-reuse mistakes, lock-order hazards, and unrelated edits.
- [x] Record the consolidated ownership and CPU invariant in the project design.
