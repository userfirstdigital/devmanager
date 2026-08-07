# Phase 3: Terminal and Process Supervision Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the durable host exclusive ownership of native PTYs and every managed process tree, with lossless terminal state, bounded client projection, truthful Task Manager-style resource accounting, safe input focus, and zero-orphan teardown.

**Architecture:** A host-owned `ProcessSupervisor` launches every managed root suspended, assigns it to a kill-on-close Windows Job Object, records ownership and generation, then resumes it. A host-owned `TerminalService` reads each PTY once into a canonical `alacritty_terminal` grid and fans snapshots/deltas to clients. Close enters the Phase 2 admission barrier before bounded concurrent teardown, and succeeds only after every owned Job proves zero members. Process inventory and resource sampling are background projections over Job membership; UI paint/input paths perform no OS enumeration.

**Tech Stack:** Rust/Tokio, portable-pty, alacritty_terminal, Windows Job Objects/completion ports/process APIs, sysinfo only where it has verified semantics, existing port probes.

## Global Constraints

- Every launched process has exactly one `ProcessOwner::{Task, Host}` before its first instruction executes.
- External processes may be observed but are never assigned, adopted, renamed, signaled, or killed by DevManager.
- `Unknown`/untracked is a reconciliation fault, not a fourth steady ownership class; steady observations are Task-owned, Host-owned, or External.
- A managed root must be created suspended, assigned to its Job Object, and only then resumed. If assignment fails, terminate the still-suspended process and report failure.
- Child membership comes from the Job Object, not a one-time parent-PID walk. Parent-PID enumeration may enrich labels but is not ownership truth.
- A cached, cycle-safe Toolhelp snapshot and per-resource environment marker may enrich foreground-process labels and recover attribution through shells, following Herdr's Windows approach; neither may grant ownership, kill authority, or override Job membership.
- Closing a terminal view or desktop client does not close its PTY. Closing a task or full quit invokes explicit supervisor teardown.
- Live client detach/reattach, host/process restart, provider exact-resume, and optional terminal-history recovery are distinct states. A restart creates a new PTY generation and must never present pre-crash process state as still live; provider conversation identity remains governed by the Phase 4 correlated-session contract.
- Once Task/host admission is `Closing`, no new root, PTY, input, service operation, or retry may enter that scope. Every completion is fenced by owner, action epoch, resource ID, generation, PID, and creation time.
- One PTY read feeds one canonical grid. Semantic and raw views do not create extra readers or provider processes.
- Keep `alacritty_terminal` as the canonical grid engine. Herdr's PTY actor and control-lease patterns are implementation references, not a dependency or terminal-engine migration; reconsider Ghostty VT only if the committed ANSI corpus demonstrates a material compatibility gap.
- CPU shown by default is process-tree CPU divided by logical processor count and clamped to `0..=100`, matching Task Manager's whole-machine convention. Raw core-equivalent percent stays diagnostics-only.
- Process enumeration, job queries, port probes, quota probes, and resource aggregation execute on scheduled workers outside terminal reads, input, layout, and paint.
- Real executable names remain unchanged. Better Task Manager attribution comes from command-line labels and in-app process trees, not executable copying or spoofing.

---

## File map

- Create: `src/process/mod.rs`
- Create: `src/process/identity.rs`
- Create: `src/process/job.rs`
- Create: `src/process/launcher.rs`
- Create: `src/process/registry.rs`
- Create: `src/process/sampler.rs`
- Create: `src/process/teardown.rs`
- Create: `src/process/ports.rs`
- Create: `src/terminal/protocol.rs`
- Create: `src/terminal/service.rs`
- Create: `src/terminal/replica.rs`
- Modify: `src/terminal/{mod,session,view}.rs`
- Refactor: `src/services/process_manager.rs`
- Refactor: `src/services/platform_service.rs`
- Refactor: `src/services/ports_service.rs`
- Modify: `src/domain/{resource,command,event,snapshot}.rs`
- Modify: `src/kernel/runtime.rs`
- Modify: `src/host/{mod,admission,shutdown}.rs`
- Modify: `src/protocol/{capabilities,envelope}.rs`
- Modify: `src/client/action.rs`
- Modify: `Cargo.toml`
- Create: `tests/process_supervisor.rs`
- Create: `tests/process_accounting.rs`
- Create: `tests/terminal_service.rs`
- Create: `tests/terminal_replication.rs`
- Create: `tests/input_routing.rs`
- Create: `tests/port_status.rs`

### Task 3.1: Define explicit process ownership and runtime identity

**Files:** `src/process/{mod,identity,registry}.rs`, `src/domain/resource.rs`, `src/kernel/runtime.rs`, `tests/process_supervisor.rs`

**Contracts:**

```rust
pub enum ProcessOwner { Task(TaskId), Host }
pub enum ProcessClassification {
    Managed(ProcessOwner),
    External,
    ReconciliationFault { reason: OwnershipFault },
}

pub struct ManagedProcessId {
    pub pid: u32,
    pub creation_time_100ns: u64,
}

pub struct LaunchIntent {
    pub resource_id: ResourceId,
    pub generation: u64,
    pub owner: ProcessOwner,
    pub kind: ManagedProcessKind,
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub display_label: String,
}
```

- [ ] **Step 1: Write failing tests** `pid_reuse_does_not_match_identity`, `resource_has_exactly_one_owner`, `stale_generation_cannot_unregister_new_process`, `external_process_cannot_become_managed`, and `launch_intent_rejects_unresolved_executable`.
- [ ] **Step 2: Run** `cargo test --test process_supervisor identity_ -- --nocapture` through the isolation wrapper and capture the red output.
- [ ] **Step 3: Implement identity comparison** using PID plus Windows creation time and canonical executable path; never use PID alone for destructive action.
- [ ] **Step 4: Implement `ProcessRegistry`** as the sole mapping from `ResourceId`/generation to Job/root identity/owner/display label. Observed non-members classify as External; a process expected to be managed but absent/mismatched becomes `ReconciliationFault`, never a steady Unknown. Registry mutation occurs on the host executor and emits resource facts through the command bus.
- [ ] **Step 5: Validate launch intents** before any OS handle is created: absolute executable, existing cwd, bounded argument/environment sizes, owner exists, and generation is current.
- [ ] **Step 6: Run** focused tests; commit as `feat(process): define managed process identity and ownership`.

### Task 3.2: Make Job Object assignment precede execution

**Files:** `src/process/{job,launcher}.rs`, `src/services/platform_service.rs`, `Cargo.toml`, `tests/process_supervisor.rs`

**Settled PTY boundary:** stock `portable-pty` 0.9.0 and current upstream do not expose the
Windows primary-thread handle or a pre-resume handoff. Carry one exact-revision,
upstreamable dependency patch that adds a typed, non-cloneable pending child. Keep the
ordinary API unchanged; do not build a parallel ConPTY adapter or a permanent terminal
harness fork. Remove the patch when the capability ships upstream.

- [ ] **Step 1: Add Windows integration tests** `child_is_suspended_until_job_assignment`, `assignment_failure_never_executes_child`, `nested_children_join_job`, `breakaway_is_disabled`, and `closing_job_terminates_entire_tree`. Use a purpose-built test helper binary that writes markers only after resumption and spawns a grandchild.
- [ ] **Step 2: Run** `cargo test --test process_supervisor launch_ -- --nocapture` and save the red result.
- [ ] **Step 3: Enable only required `windows` crate features** for Foundation, Threading, JobObjects, IO completion, process status, and security. Keep platform code behind `cfg(windows)` with a typed unsupported error elsewhere.
- [ ] **Step 4: Implement launch order:** create Job with `KILL_ON_JOB_CLOSE`; pass it through `PROC_THREAD_ATTRIBUTE_JOB_LIST`; create the ConPTY root with `CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT`; register the process handle plus PID/creation-time/canonical-executable identity; then consume the pending child with exactly one `resume()`. Creation-time Job membership prevents an uncontained child even if the host fails before registration.
- [ ] **Step 5: Keep the dependency patch narrow:** expose only `spawn_command_suspended_in_job`, process ID/handle access, consuming `resume`, and fail-safe `abort_and_wait`; pin its upstream base commit and retain the MIT notice.
- [ ] **Step 6: On any pre-resume failure**, terminate the still-suspended root, close handles, remove the provisional registry entry, and emit a failed launch fact. Never fall back to an uncontained launch.
- [ ] **Step 7: Preserve provider/terminal command lines** and add `DEVMANAGER_TASK_ID`, `DEVMANAGER_RESOURCE_ID`, and a concise display label as environment/arguments only where the child contract permits it.
- [ ] **Step 8: Run** launch tests and inspect marker files/PIDs; include credential-free Claude, Codex, and Cursor `--version` PTY smoke checks; commit as `feat(process): assign jobs before managed code executes`.

### Task 3.3: Track complete Job membership with completion notifications

**Files:** `src/process/{job,registry}.rs`, `tests/process_supervisor.rs`

- [ ] **Step 1: Write failing tests** where a root exits before its grandchild, children fork rapidly, PIDs are reused in a fake registry, and completion messages arrive after a replacement generation.
- [ ] **Step 2: Run** `cargo test --test process_supervisor membership_ -- --nocapture` and record the red result.
- [ ] **Step 3: Associate each Job Object with one IO completion port** and translate new-process, exit-process, abnormal-exit, active-process-zero, and limit events into generation-fenced supervisor messages.
- [ ] **Step 4: Query active PIDs from the Job** for reconciliation, enrich them with creation time/executable/command line when accessible, and retain inaccessible members as counted unknown members rather than dropping them.
- [ ] **Step 5: Define terminal states** `Starting`, `Running`, `Stopping`, `Stopped`, `Failed`, and `Leaked`. Transition to `Stopped` only after `ACTIVE_PROCESS_ZERO`; root exit alone is insufficient.
- [ ] **Step 6: Run** the membership tests repeatedly (`1..20 | % { cargo test --test process_supervisor membership_ --quiet }`) and commit as `feat(process): track complete job membership`.

### Task 3.4: Move PTY ownership and canonical terminal grids into the host

**Files:** `src/terminal/{mod,session,protocol,service}.rs`, `src/host/mod.rs`, `src/domain/resource.rs`, `tests/terminal_service.rs`

**Interface:**

```rust
pub struct TerminalService {
    // Host-owned registry and executors; no presentation types.
}

impl TerminalService {
    pub async fn create(&self, owner: TaskId, spec: TerminalSpec) -> Result<TerminalId, TerminalError>;
    pub async fn write(&self, id: TerminalId, input: InputEnvelope) -> Result<InputAck, TerminalError>;
    pub async fn resize(&self, id: TerminalId, size: TerminalSize) -> Result<(), TerminalError>;
    pub async fn snapshot(&self, id: TerminalId) -> Result<TerminalSnapshot, TerminalError>;
    pub async fn close(&self, id: TerminalId, reason: CloseReason) -> Result<TeardownReport, TerminalError>;
}
```

- [ ] **Step 1: Add failing tests** `terminal_survives_client_disconnect`, `host_restart_never_reuses_live_pty_generation`, `provider_resume_identity_is_independent_of_pty_generation`, `one_pty_reader_feeds_canonical_grid`, `ansi_vt_corpus_matches_expected_grid`, `utf8_split_across_reads_is_preserved`, `wide_and_combining_cells_round_trip`, `cursor_modes_and_alternate_screen_round_trip`, `bracketed_paste_and_mouse_modes_round_trip`, `osc8_links_are_sanitized`, `osc52_requires_clipboard_policy`, `resize_is_serialized_with_output`, and `terminal_close_waits_for_job_zero`.
- [ ] **Step 2: Run** `cargo test --test terminal_service -- --nocapture` and capture the red output.
- [ ] **Step 3: Refactor `TerminalSession`** into host-only PTY handles, a single reader task, bounded input channel, and canonical alacritty grid. Remove GPUI window/entity ownership from the session core.
- [ ] **Step 4: Route terminal launch through `ProcessSupervisor`** so PTY root and descendants share the Task-owned Job. Store only restart recipe/metadata durably; PTY handles and parser grids remain runtime state.
- [ ] **Step 5: Serialize output, resize, title, bell, working-directory markers, clipboard/link requests, and exit into one per-terminal sequence.** Treat OSC/title/link/clipboard data as untrusted, validate protocols/lengths, require explicit clipboard policy, and emit lightweight resource facts without persisting raw terminal bytes by default.
- [ ] **Step 6: Run** terminal service tests and commit as `refactor(terminal): move pty and grid ownership to host`.

### Task 3.5: Add terminal snapshots, deltas, and per-client viewport state

**Files:** `src/terminal/{protocol,replica,view}.rs`, `src/protocol/{capabilities,envelope}.rs`, `src/host/connection.rs`, `src/client/model.rs`, `tests/terminal_replication.rs`

- [ ] **Step 1: Write failing tests** `snapshot_then_deltas_matches_host_grid`, `gap_requests_snapshot`, `slow_client_coalesces_to_snapshot`, `two_clients_have_independent_scroll_offsets`, `selection_and_search_are_client_local`, `bounded_scrollback_emits_truncation_marker`, `long_scrollback_memory_is_bounded`, `authorized_devices_alternate_without_control_lease`, `watcher_input_is_rejected_by_permission`, `concurrent_resize_last_valid_view_sequence_wins`, and `disconnect_clears_resize_preference`.
- [ ] **Step 2: Run** `cargo test --test terminal_replication -- --nocapture` and record the red result.
- [ ] **Step 3: Define** `TerminalSnapshot { terminal_id, generation, sequence, size, cursor, modes, title, rows }` and compact `TerminalDelta` operations for changed rows, scroll, cursor, mode, title, and exit.
- [ ] **Step 4: Implement `TerminalReplica`** in client space. It applies only contiguous deltas for matching generation; any gap/generation mismatch clears pending operations and requests a snapshot.
- [ ] **Step 5: Keep scroll offset, selection, hover, search/find state, and copy mode per client.** Bound host scrollback by rows/bytes, surface an explicit truncation marker, and keep only PTY size shared. Track an ephemeral `TerminalViewPreference { terminal_id, terminal_generation, client_id, view_sequence, visible_size }` only to choose and debounce the latest valid visible resize; disconnect/revocation clears it. It grants no input authority, cannot block another authorized owner device or collaborator, and never becomes durable device ownership. Watchers remain read-only because of their capability grant, not because another client holds control.
- [ ] **Step 6: Put deltas on the bulk lane**, coalesce dirty rows under pressure, and guarantee that receipts/approval events stay deliverable.
- [ ] **Step 7: Run** replication tests including a 10 MB output fixture; commit as `feat(terminal): replicate canonical terminal state`.

### Task 3.6: Sequence input and eliminate click-through across task switches

**Files:** `src/terminal/protocol.rs`, `src/client/model.rs`, `src/terminal/view.rs`, `tests/input_routing.rs`

- [ ] **Step 1: Write failing tests** for monotonically sequenced input, duplicate input retry, stale-focus rejection, sidebar mouse-down followed by terminal mouse-up, task switch while a choice prompt is visible, and IME/paste delivery after focus confirmation.
- [ ] **Step 2: Run** `cargo test --test input_routing -- --nocapture` and retain the red result.
- [ ] **Step 3: Define `InputEnvelope`** with `client_id`, `input_id`, `terminal_id`, `terminal_generation`, `focus_epoch`, and bytes. The host permission-checks each mutation independently, deduplicates `input_id`, atomically assigns the next per-terminal accepted sequence, and acknowledges accepted/duplicate/rejected input. There is no client control lease: any authorized owner device or collaborator may submit the next input, while stale generation/focus or read-only grants are rejected without forwarding bytes to the PTY.
- [ ] **Step 4: Increment `focus_epoch`** on task/view switch and require a completed click sequence inside the terminal content after activation before mouse-derived terminal input is eligible.
- [ ] **Step 5: Consume the navigation click** at the sidebar/shell layer; clear hover/pressed terminal state on deactivate; reject queued input for an older focus epoch. Keyboard focus may be deliberately restored after activation without synthesizing Enter or mouse coordinates.
- [ ] **Step 6: Preserve terminal protocol bytes exactly** for keyboard, paste, mouse reporting, and IME commit; semantic answer controls issue provider commands rather than fabricated terminal clicks.
- [ ] **Step 7: Run** the focus/input suite and commit as `fix(terminal): fence input across task switches`.

### Task 3.7: Implement graceful, escalating, verifiable teardown

**Files:** `src/process/teardown.rs`, `src/terminal/service.rs`, `src/host/{admission,shutdown}.rs`, `tests/process_supervisor.rs`

- [ ] **Step 1: Add failing tests** for admission closing before cooperative exit, launch/input race after close begins, cooperative exit, ignored close request, child surviving root exit, cancellation during teardown, simultaneous task close/full quit, duplicate dispose, bounded concurrent teardown, timeout resulting in active-process-zero after Job termination, and residue preventing a `Closed` event.
- [ ] **Step 2: Run** `cargo test --test process_supervisor teardown_ -- --nocapture` and save the red output.
- [ ] **Step 3: Preserve distinct shared ActionCatalog entries:** foreground interrupt, terminal close, and terminate managed tree remain separate commands. Task/full-host close first calls the Phase 2 admission barrier; only the winning operation schedules cleanup.
- [ ] **Step 4: Implement teardown stages in dependency order:** cancel/drain terminal writers and supervisor jobs; send provider-specific cooperative close/close stdin where applicable; wait bounded grace; request console/process close when safe; wait again; terminate each Job; wait for `ACTIVE_PROCESS_ZERO`; detach listeners/PTY handles; reconcile ports; persist branch reports; settle the close operation.
- [ ] **Step 5: Run independent resource branches through a fixed-size executor** (default four, configurable only in tests), not one unbounded future per process. Preserve deterministic settlement ordering by resource ID even when branches complete concurrently.
- [ ] **Step 6: Keep Job/port/process handles alive** until zero-members confirmation. A timeout after forced termination becomes `CleanupFailed`/`Leaked` with Task/resource, Job name, PID plus creation time, executable/command label, last lifecycle event, and attempted stages; it blocks `Closed`.
- [ ] **Step 7: Make teardown idempotent** and merge concurrent close reasons onto the original OperationId. Full quit awaits all Task-owned and Host-owned reports before releasing the host lock.
- [ ] **Step 8: Run** teardown tests and verify helper PIDs are absent; commit as `feat(process): add admission first zero member teardown`.

### Task 3.8: Report complete process trees and Task Manager-style CPU

**Files:** `src/process/sampler.rs`, `src/services/process_manager.rs`, `src/app/process_monitor.rs`, `src/domain/snapshot.rs`, `tests/process_accounting.rs`

**Metrics:**

```text
raw_core_percent = 100 * delta_process_cpu_time / delta_wall_time
task_manager_percent = clamp(raw_core_percent / logical_processor_count, 0, 100)
tree_cpu = sum(unique Job-member deltas once per process identity)
```

- [ ] **Step 1: Write failing deterministic tests** for one saturated core on 8 logical processors (`12.5%`), eight saturated cores (`100%`), invalid/zero intervals, PID reuse, inaccessible child, overlapping ancestry deduplication, process exit between samples, and memory summation without double count.
- [ ] **Step 2: Run** `cargo test --test process_accounting -- --nocapture` and retain the red result.
- [ ] **Step 3: Sample cumulative kernel+user time** for every unique Job member on a fixed background cadence; key previous values by PID plus creation time. Do not use instantaneous per-process percentages with incompatible denominators.
- [ ] **Step 4: Expose both** `machine_cpu_percent` (`0..=100`) and diagnostic `core_equivalent_percent`; label them unambiguously. The top bar/list defaults to the former.
- [ ] **Step 5: Sum private working set and optional I/O deltas** across unique members. Include inaccessible members in process count with `metrics_unavailable=true` so totals are transparently partial.
- [ ] **Step 6: Replace window-terminal attribution** with `ResourceId`/Job ownership. Show executable, command-line label, Task, resource kind, PID, child count, CPU, memory, and state in the in-app process tree.
- [ ] **Step 7: Run** accounting tests and one controlled CPU helper comparison against Task Manager; commit as `fix(process): report complete trees with task manager cpu math`.

### Task 3.9: Distinguish managed, starting, external, and stopped ports

**Files:** `src/process/ports.rs`, `src/services/ports_service.rs`, `src/domain/snapshot.rs`, `tests/port_status.rs`

- [ ] **Step 1: Write failing tests** for `Starting` orange, managed healthy green, externally occupied blue, stopped gray, probe failure distinct from free, PID reuse, and cached scans never running on the render/input thread.
- [ ] **Step 2: Run** `cargo test --test port_status -- --nocapture` and record the red result.
- [ ] **Step 3: Build one scheduled port inventory** that snapshots listeners/PIDs outside the hot path, joins listener identity to the process registry, and publishes immutable status snapshots.
- [ ] **Step 4: Define precedence:** managed launch in progress = orange; listener owned by matching managed generation and health-ready = green; listener exists but is not owned by that resource = blue; no listener and no launch = gray; probe error = explicit unknown/error.
- [ ] **Step 5: Never kill or adopt a blue listener.** A requested managed start on an occupied external port fails with owner evidence and leaves the external process untouched.
- [ ] **Step 6: Run** port tests with a real temporary listener and commit as `feat(process): surface external listeners in blue`.

### Task 3.10: Stress process/terminal ownership to zero orphans

**Files:** `scripts/native-next/Invoke-ProcessSoak.ps1`, `tests/{process_supervisor,terminal_service,terminal_replication}.rs`, `docs/replacement-deletion-ledger.md`

- [ ] **Step 1: Add helper modes** for rapid fork/exit, large output, ignored cooperative close, grandchild lifetime, CPU load, memory load, and port listening to the integration-test helper binary.
- [ ] **Step 2: Create a red soak assertion** that checks Job member count, host handles, terminal sequences, and exact helper identities after 100 launch/write/resize/detach/reattach/close cycles.
- [ ] **Step 3: Implement `Invoke-ProcessSoak.ps1`** around the real host/client binaries and Phase 0 production guards. Randomize client disconnects and task-close timing with a recorded seed.
- [ ] **Step 4: Require at the end:** zero Job members, zero helper/provider children, zero owned listeners, no named pipes except the active host pipe before full quit, no leaked PTY handles, and bounded host memory/handle growth.
- [ ] **Step 5: Run shared conformance baseline/variant cases** for launch-to-first-output, input acknowledgement, stop/close settlement latency, task-switch input fencing, process/port residue, CPU denominator, and terminal resync. Record only sanitized commands/helper identities and declared metrics.
- [ ] **Step 6: Update the deletion ledger** for every old process/terminal ownership path now superseded; do not delete them until parity/cutover gates say so.
- [ ] **Step 7: Run** the soak and all focused tests; commit as `test(process): prove terminal tree cleanup under stress`.

## Phase 3 verification gate

- [ ] Capture the production baseline and tell the user that Rust test executables will appear under the isolated target directory during this long gate.
- [ ] Run `cargo test --test process_supervisor --test process_accounting --test terminal_service --test terminal_replication --test input_routing --test port_status -- --nocapture` through the Phase 0 gate.
- [ ] Run `pwsh scripts/native-next/Invoke-ProcessSoak.ps1 -Iterations 100 -Seed 3403`.
- [ ] Run `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`.
- [ ] Run the committed ANSI/VT/Unicode/OSC/clipboard/scrollback corpus and record PTY-byte-to-grid latency plus long-scrollback memory against `docs/performance-budgets.md`.
- [ ] Compare controlled CPU helper output with Task Manager and retain evidence of logical processor count, interval, raw core equivalent, and displayed percentage.
- [ ] Inspect every managed Job after close and require active process count zero.
- [ ] Rebuild the conformance query index and compare process/terminal baseline/variant arms; no dashboard-specific metric may appear without a case definition.
- [ ] Confirm externally occupied test ports were never assigned, signaled, or closed.
- [ ] Confirm no Cargo, rustc, test harness, helper, development provider, browser helper, or host remains.
- [ ] Compare production hashes and installed PID/start time; review the complete Phase 3 diff/deletion ledger.

## Phase 3 exit criteria

- Every managed root is assigned to its owner Job before execution and every descendant remains accounted for.
- The host owns one PTY read and canonical grid per terminal; clients reproduce it through snapshots/deltas.
- Switching tasks or views cannot turn a navigation click into terminal/provider input.
- Closing a task/full host rejects new work first, performs bounded idempotent teardown, yields Job `ACTIVE_PROCESS_ZERO`, and reports any residue without claiming `Closed`.
- CPU and memory totals cover unique complete process trees, and default CPU matches whole-machine Task Manager math.
- Managed/external port state is accurate without adding hot-path enumeration or affecting external processes.
