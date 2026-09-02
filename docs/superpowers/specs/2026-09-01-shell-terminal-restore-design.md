# Shell Terminal Restore Design

**Date:** 2026-09-01
**Status:** Approved design, awaiting implementation plan
**Sub-project:** 2 of 3 (depends on `2026-09-01-task-shell-terminals-design.md`)
**Related:** `2026-09-01-terminal-screen-history-design.md`, `2026-09-01-port-ideas-audit.md`

## 1. Problem

When `devmanager-host` restarts (full quit, update, crash), every managed PTY dies with its kill-on-close Job. Provider terminals come back through the provider restore lane and `--resume`. Plain shells (sub-project 1) have no restore path, and today's kernel never reconciles Terminal resources left `Active` by a dead host: they stay `Active` forever with no runtime behind them.

This design recreates plain shells after a host restart, in their last known directory, once a client attaches, and makes boot reconcile stale terminal resources honestly.

## 2. Goals and non-goals

Goals:

- After a host restart, every plain shell of an open task comes back as a fresh shell in its last reported cwd, with the same resource id, title, strip position and geometry.
- Boot marks stale terminals `Unknown` until enumeration completes, and finalizes exited or releasing ones.
- One idempotent verb, `ensure_running`, is the only way a plain shell is spawned, for open and for restore alike.
- A restore failure is isolated to its terminal.

Non-goals:

- Resurrecting running commands. The shell process is new.
- Replaying screen contents (sub-project 3 seeds through the same verb).
- Restoring shells of closed tasks.
- Surviving a host crash with live processes (kill-on-close Jobs are deliberate; see the architecture doc).

## 3. Boot reconciliation

At host boot, after the existing pre-bind gate (`HostCleanupWorker::restart_disposition`):

1. Load every Terminal resource with lifecycle `Active` or `Releasing` whose `runtime_generation` belongs to a previous boot (`KernelStore::load_recovering_resources`, today unused, becomes live).
2. Install each as `TerminalRuntimeState::Unknown` in the terminal service so clients see "unknown", not "not started".
3. Finalize with `ResourceReleased { reason: HostRestart }`: every `Releasing` resource, and every `Active` resource carrying a `TerminalExited` fact. Nothing resurrects an exited shell.
4. Publish readiness `Unknown` until enumeration (below) succeeds once.

## 4. Enumeration

`CommandBus::restorable_shell_terminals(limit)` selects plain Terminal resources (launch recipe present; no `agent_sessions.provider_resource_id` referrer) of open tasks, lifecycle `Active`, no `TerminalExited` fact, ordered by the task's durable strip order then task `updated_at_ms`.

Each `RestoreShellIntent` carries: `task_id`, `resource_id`, the recorded `TerminalLaunch`, the last `TerminalCwdReported` (if any), recorded `cols`/`rows`, `expected_task_revision`, `expected_action_epoch`.

Enumeration runs once per boot; only a successful enumeration clears the pending flag (the provider lane's rule). Failure keeps readiness `Unknown`, logs the error once per distinct message, and retries on the next reaper tick.

## 5. `ensure_running`

```rust
fn ensure_running(&mut self, resource_id: ResourceId, seed: Option<ScreenHistory>) -> Result<TerminalAttachment, RestoreError>
```

- If a live runtime exists for the resource, return it (no-op).
- Otherwise verify the resource is `Active`, plain, and fenced (task revision, action epoch), then spawn from the recorded `program`/`args` at the recorded `cols`/`rows` (never a default 24x80).
- Cwd ladder, first rung that is an existing directory:
  1. last reported cwd;
  2. nearest existing ancestor of it that is still inside the task's workspace root;
  3. the task's runtime working directory;
  4. the project's configured root.
- Emit `TerminalRestored { resource_id, cwd_used, fallback: None | AncestorOfReported | TaskRuntimeDir | ProjectRoot, at }`.
- `seed`, when present (sub-project 3), is fed into the grid before the PTY child spawns.

`OpenShellTerminal` registers the resource and then calls `ensure_running`, so open and restore cannot drift. A greyed exited shell's Restart chip action also calls it.

## 6. Restore lane

Mirrors `queue_one_provider_restore`:

- Queue of `RestoreShellIntent`, deduplicated by `resource_id` (not task id).
- Shared concurrency budget with provider restores: `MAX_CONCURRENT_RESTORES = 2`, providers dequeued first.
- Negative cache keyed by `(resource_id, action_epoch)`: a failed restore is not retried until the task changes.
- Fence check immediately before spawning; `StaleFence` drops the intent silently and re-enumerates on the next tick.
- Trigger: the lane starts pumping when the first client subscribes to any task snapshot after boot. It never spawns into a host nobody has attached to.
- Failure records `TerminalExited { code: None, summary: "restore failed: <stage or cwd reason>" }` on that resource only. The chip stays visible, greyed, with the reason and a Restart action.

## 7. Host quit inspection

`inspect_host_quit` adds `shells: Vec<{ resource_id, task_id, title, label }>` beside the agents and resources it already lists, so a full quit names what it would end. `authorize_full_host_quit` continues to fail closed on any active resource.

## 8. Client UX

- After restart, each shell chip shows `restoring` until `ensure_running` settles, then live. Tooltip: "restored <time>" and, when the directory moved, "opened in <dir> because <reason>".
- Without sub-project 3 the restored shell starts with a clean screen. Strip order, focus, titles and cwd come back.
- Chip states: `unknown`, `restoring`, `live`, `exited` (with Restart).

## 9. Error handling

| Condition | Behaviour |
| --- | --- |
| Enumeration fails | Readiness stays `Unknown`; strip shows unknown chips; retry next tick; log once per message. |
| Fence stale | Intent dropped; re-enumerated. |
| Spawn failure | `TerminalExited` with launcher stage; chip greyed with Restart. |
| No cwd rung exists | Rung 4 (project root) is created by configuration and always exists; if it does not, `TerminalExited { summary: "no usable directory" }`. |
| Reader/wait actor failure after restore | Governed by `39caf754` and `c99a0c5f`. |

## 10. Testing

- Kernel: `restorable_shell_terminals` selects plain shells only; skips exited, releasing, closed-task and provider terminals; orders by strip; boot finalization emits `ResourceReleased { HostRestart }` exactly once per stale resource; replaying the store after a simulated restart yields identical intents.
- Lane: dedupe by resource id; shared budget with provider restores; fence rejection; negative cache keyed by action epoch; no spawn before the first client subscription.
- `ensure_running`: idempotent on a live runtime; each cwd rung exercised with a temp directory tree; recorded geometry used; `TerminalRestored` names the fallback.
- Process integration: launch a real shell, kill the host process, start a new host, subscribe as a client, assert the shell is respawned in the last reported cwd with the same resource id and title.
- UI: chip states unknown, restoring, live, exited-with-restart.
- Sabotage: remove the first-subscribe gate and confirm the no-spawn-before-client test fails; remove the exit-fact filter and confirm the exited-shell test fails.

## 11. Decisions log

- Launch on first client attach, not eagerly at boot, and not lazily per task.
- Fresh shell in last cwd; no command resurrection.
- Only plain shells of open tasks; providers keep their lane; services keep their supervisor.
- `ensure_running` follows traycer's `ensureRunning`; the cwd ladder replaces herdr's `$HOME`/`/` fallback with task-scoped rungs; recorded geometry replaces herdr's 24x80 restore.
- Kill-on-close Jobs stay: a crashed host still ends its terminals, by design.
