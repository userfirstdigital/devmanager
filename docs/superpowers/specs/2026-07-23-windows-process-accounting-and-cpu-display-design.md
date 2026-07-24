# Windows Process Accounting and CPU Display Design

**Date:** 2026-07-23
**Status:** Implemented and verified
**Scope:** Make DevManager account for the full process set owned by a terminal session on Windows, keep detached descendants visible and reapable, use Task Manager-style CPU percentages, and show safe role labels for known subprocesses.

## 1. Problem

DevManager currently discovers a terminal's processes by walking live parent-PID links from the terminal root. That works only while every intermediate launcher remains alive. Toolchains such as `npm -> shell -> node -> Vitest worker` can lose an intermediate process while the workers continue running, at which point the workers disappear from DevManager's monitor even though they still consume memory.

The persisted PID ledger does not repair that gap because each refresh replaces the previous descendant set with the descendants visible in the current parent walk. Windows terminal sessions also attempt to use a Job Object, but DevManager neither queries that object for active members nor reports an attach failure.

CPU values are currently stored and displayed directly from `sysinfo::Process::cpu_usage()`. Those values are core-scaled, so one fully used logical CPU is `100%` and a multi-process tree can exceed `100%`. Windows Task Manager instead presents a process as a share of total machine capacity.

Raw Windows process names are not useful for Node-heavy trees. Windows Task Manager cannot be given arbitrary per-process display names without changing the executable, but DevManager can safely classify known command shapes in its own monitor.

## 2. Goals

- Include active Windows Job Object members in every session resource sample.
- Keep previously verified descendants in the sample and PID ledger after a parent link breaks.
- Preserve parent-walk discovery as the cross-platform and Job Object failure fallback.
- Ensure process-monitor kill authorization and cleanup use the same owned PID truth as reporting.
- Store and display CPU as a whole-machine percentage in the `0..=100` Task Manager convention.
- Retain the logical CPU count so the UI can derive and explain equivalent cores without storing a second CPU truth.
- Replace generic `node.exe` labels with a small allowlisted set of safe roles such as `Vitest worker`, `Vitest`, `Context7 MCP`, `npm`, and `npx`.
- Never persist or render raw command lines, arguments, environment variables, tokens, or secrets.

## 3. Non-goals

- Renaming executables in Windows Task Manager.
- Editing Claude Code, Context7, Vitest, or CommandIT configuration.
- Adding a user-selectable CPU math mode.
- Introducing a second process registry outside the existing runtime snapshot, managed Job Object, and PID ledger.
- Guaranteeing ownership of processes that escaped both the Job Object and the parent tree before DevManager ever observed them.

## 4. Ownership model

For a live terminal session, DevManager builds one ordered PID set from:

1. the verified runtime or ledger root;
2. all currently visible descendants found by parent traversal;
3. all active PIDs reported by the session's Windows Job Object;
4. previously recorded descendant identities that still match PID, start time, and process name.

The root remains first and all other PIDs are deduplicated and deterministically sorted. A PID is sampled only if it exists in the current `sysinfo::System` snapshot. Previously recorded identities are never trusted by PID alone, preventing PID reuse from granting monitor or kill authority.

On Windows, Job Object membership is the strongest live ownership signal because it survives an intermediate parent exit. If Job Object creation, assignment, or query is unavailable, DevManager logs a concise diagnostic and continues with verified ancestry plus ledger identities. Non-Windows behavior continues to use ancestry plus the verified ledger.

Every successful sample writes the complete verified descendant set back to the existing ledger. A sync must therefore retain a prior identity while it is still verified, rather than erasing it merely because its current parent chain is broken.

## 5. Windows Job Object query

`ManagedProcessJob` exposes an active-PID query backed by `QueryInformationJobObject` with `JobObjectBasicProcessIdList`. The implementation grows its aligned buffer when Windows reports more members than fit and returns a sorted, deduplicated list.

`TerminalSession` exposes only the PID query result; it does not expose the raw Job handle. `ProcessManager` snapshots the relevant `Arc<TerminalSession>` references before sampling so it does not hold the sessions mutex while querying Windows or refreshing process metadata.

The existing `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` behavior remains unchanged. Query failure degrades reporting but does not prevent a terminal from launching.

## 6. CPU contract

For each sampled process:

```text
system_cpu_percent = core_scaled_cpu_percent / logical_cpu_count
```

The value is sanitized for non-finite or negative input and clamped to `0..=100`. Session CPU is the normalized sum of its sampled processes, also clamped to `0..=100`.

`ResourceSnapshot.cpu_percent` and `ProcessResourceNode.cpu_percent` now mean whole-machine percentage everywhere. `ResourceSnapshot.logical_cpu_count` records the divisor and defaults safely for snapshots produced by older versions.

Equivalent cores are always derived:

```text
equivalent_cores = system_cpu_percent * logical_cpu_count / 100
```

Compact surfaces show the normalized percentage. The expanded process monitor shows both, for example `2.0% system · 1.25 cores`, so high parallelism remains understandable without returning to a percentage above `100`.

## 7. Safe process labels

The sampler refreshes command metadata only for classification and immediately reduces it to an allowlisted role. Matching is case-insensitive and based on executable/argument basenames or known package tokens. No raw command text is copied into runtime state.

Initial classifications:

- `tinypool` worker entry -> `Vitest worker`
- Vitest entry -> `Vitest`
- `@upstash/context7-mcp` or `context7-mcp` -> `Context7 MCP`
- `npm-cli.js` -> `npm`
- `npx-cli.js` -> `npx`
- otherwise -> normalized OS process name

## 8. Failure handling

- Job attach failure: keep the terminal running, emit a diagnostic, and use ancestry plus verified ledger identities.
- Job query failure: keep the previous verified ledger safety net, emit a diagnostic, and continue sampling.
- Missing root with verified descendants: continue to show the session as unreaped using the active ledger identities.
- PID reuse: discard the stale identity.
- Unavailable command line: fall back to the OS process name.
- Zero/unknown logical CPU count: use one as the safe divisor.

## 9. Verification

- Pure tests prove CPU normalization, clamping, and equivalent-core derivation.
- Pure tests prove PID union keeps Job members and verified detached ledger members without accepting stale identities.
- PID-ledger tests prove a sync cannot erase a still-running detached descendant.
- Classifier tests prove known labels and confirm that arbitrary command arguments never become display labels.
- Windows integration tests prove a managed Job reports an assigned process and a spawned child.
- Existing process-monitor tests prove normalized values and core details reach the view model.
- Focused tests, full Rust tests, formatting, clippy/check/build, and a Windows runtime process harness must pass before completion.

## 10. Verified outcome

Implemented on 2026-07-23 and reverified on 2026-07-24 after integration with the persistent-pairing and storage-isolation work. The permanent Windows regression harness proves that a worker remains discoverable through Job membership after its intermediate launcher exits, and cleanup leaves neither a worker nor a temporary directory behind.

Verification completed with:

- focused process-accounting suite: 115 passed;
- targeted post-review regressions: 10 passed;
- complete serial library suite: 987 passed, 1 ignored;
- `cargo fmt -- --check`;
- `cargo clippy --all-targets --all-features` (exit 0, with the repository's existing warning baseline);
- `cargo build --all-features`;
- `git diff --check`;
- production-state guards confirming the installed DevManager process and production remote-state file were unchanged throughout verification.

The durable invariant is: session process truth is the union of Windows Job membership, live ancestry, and identity-verified ledger entries; PID alone never grants ownership. CPU percentages stored in runtime state are whole-machine percentages, and equivalent cores are derived rather than stored as a second truth.
