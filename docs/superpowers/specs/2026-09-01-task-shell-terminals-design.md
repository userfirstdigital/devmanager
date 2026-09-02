# Task Shell Terminals Design

**Date:** 2026-09-01
**Status:** Approved design, awaiting implementation plan
**Sub-project:** 1 of 3 (shells in tasks → shell restore → screen history)
**Related:** `2026-09-01-shell-terminal-restore-design.md`, `2026-09-01-terminal-screen-history-design.md`, `2026-09-01-port-ideas-audit.md`

## 1. Problem

The host-based DevManager has no plain shell terminals. `ProcessManager::spawn_shell_session` has no production caller, the `OpenTerminal` action only toggles the provider terminal's visibility, and the services panel's terminal action is inert. The durable `ResourceRecipe::Terminal` records only `cols` and `rows`, and `TerminalService` rejects a task with more than one live terminal. Restoring shells after a host restart (sub-project 2) and replaying their screens (sub-project 3) both presuppose that plain shells exist and are recorded durably.

This design adds one or more plain shell terminals per task, recorded in the kernel with everything needed to recreate them, shown beside the provider terminal in the task cockpit.

## 2. Goals and non-goals

Goals:

- Open, rename, reorder, focus and close plain shells inside a task.
- Record durably per shell: title, launch cwd, resolved shell program and args, geometry, live cwd, exit state, created and last-activity timestamps; per task: terminal order and focused terminal.
- Keep the provider terminal's identity, fences and code paths unchanged.
- Keep `ResourceId` as the only terminal identity. Cwd, PID, timestamps and titles are recorded facts, never identity (master design rule).

Non-goals:

- Restoring shells after a host restart (sub-project 2).
- Screen history (sub-project 3).
- Shells on the Connect mobile/web client (later slice; Connect stays on the provider terminal).
- Split views. One terminal is visible at a time behind a strip.
- Configured service terminals; they keep their supervisor semantics.

## 3. Durable model

### 3.1 Terminal recipe

`ResourceRecipe::Terminal` gains two optional fields:

```rust
ResourceRecipe::Terminal {
    cols: u16,
    rows: u16,
    launch: Option<TerminalLaunch>,   // None = provider-owned terminal (today's shape)
    title: Option<String>,            // user-set, renamable
}

struct TerminalLaunch {
    cwd: PathBuf,        // validated existing directory at open time
    program: PathBuf,    // resolved absolute shell executable
    args: Vec<String>,   // resolved at open time
}
```

The host resolves `program` and `args` from the `DefaultTerminal` setting when the shell is opened and records the resolved values. A later settings change never changes an existing terminal.

A Terminal resource is a **plain shell** when `launch` is present. It is a **provider terminal** when `launch` is absent; the existing `agent_sessions.provider_resource_id` binding remains the authoritative link for those.

`ResourceRecipeWire` currently uses `deny_unknown_fields`. The recipe wire version is bumped; the decoder accepts the two new optional fields, defaults them when absent, and continues to reject unknown fields. A recipe that cannot decode is reported as a reconciliation fault on that resource, never as store corruption.

### 3.2 Per-terminal facts

New durable events, all keyed by `ResourceId`:

| Event | Emitter | Meaning |
| --- | --- | --- |
| `TerminalRenamed { resource_id, title }` | command | user rename |
| `TerminalCwdReported { resource_id, cwd, at }` | host | live cwd changed (debounced 2 s, suppressed when unchanged) |
| `TerminalExited { resource_id, code: Option<i32>, summary, at }` | host | the shell's root process exited, or a spawn/restore failed |
| `TerminalActivity { resource_id, at }` | host | last-activity timestamp, coalesced to at most one per 30 s per terminal |

`ResourceRegistered` carries `created_at` already via `updated_at_ms`; the projection exposes it as `created_at`.

### 3.3 Per-task strip fact

```rust
Event::TaskTerminalStripSet { task_id, order: Vec<ResourceId>, focused: Option<ResourceId> }
```

Replaces the whole list each time. Validation: no duplicates; `focused` must be in `order`; every id must be a Terminal resource of that task. The provider terminal is always rendered first regardless of `order`; `order` governs the plain shells.

### 3.4 Limits

At most 8 plain shells per task, enforced by the command validator before registration.

### 3.5 Projection

The `resources` table keeps its shape; the recipe blob carries the new fields. New projection table `terminal_facts (resource_id PK, title, live_cwd, exit_code, exit_summary, exited_at, last_activity_at)` and `task_terminal_strip (task_id PK, order_json, focused_resource_id)`. Both are rebuilt from events like every other projection.

## 4. Commands and host behaviour

All commands are task mutations fenced on task revision and action epoch.

| Command | Host behaviour |
| --- | --- |
| `OpenShellTerminal { task_id, cwd: Option<PathBuf> }` | Resolve cwd (given, else the task workspace's runtime working directory). Validate it is an existing directory. Resolve the shell candidate list from `DefaultTerminal` (pwsh, powershell, cmd on Windows). Reject if the task already has 8 plain shells. Register the resource with the launch recipe, append it to the strip, then spawn. |
| `CloseTerminal { resource_id }` | Existing release path: `ResourceReleaseBegun` → managed teardown → `ResourceReleased`. Removes the id from the strip and deletes any sidecar files. |
| `RenameTerminal { resource_id, title }` | Emits `TerminalRenamed`. Title trimmed, 1..=64 chars. |
| `SetTerminalStrip { task_id, order, focused }` | Emits `TaskTerminalStripSet` after validation. |

Spawning reuses the suspended-in-Job managed launcher and the existing reader and wait actors. New environment markers: `DEVMANAGER_TASK_ID` (already present for providers) and `DEVMANAGER_RESOURCE_ID`. Environment is otherwise derived by the host; clients never supply env.

### 4.1 Live cwd ladder

1. OSC 7 / OSC 9;9 report parsed from the PTY stream.
2. Root process current directory read from its PEB, using one shared process snapshot per sampler tick (never per terminal).
3. Nothing: keep the last recorded value.

PowerShell (`pwsh.exe`, `powershell.exe`) is launched with a `-NoExit -Command` prompt hook that emits OSC 9;9 with the provider path and updates `[Environment]::CurrentDirectory`, because PowerShell does not update its Win32 cwd on `Set-Location`. The hook calls the original prompt first so `$?` survives. The user's profile is never edited. `cmd.exe` relies on rung 2.

A reported cwd is accepted only if absolute and an existing directory.

### 4.2 Server-computed label

From the same shared process snapshot, each terminal gets a label: the running child command name when a child is running, else the shell name. Capped at 128 chars. Ephemeral, never persisted.

## 5. Terminal service and queries

`TerminalService` becomes keyed by `ResourceId`.

- One entry per resource: alacritty grid, replay buffer, generation, sequence, and `TerminalRuntimeState::{ Running, Exited { code }, Unknown }`. `Unknown` is used only between host boot and the completion of restore enumeration (sub-project 2). It is never a synonym for stopped.
- The provider terminal is the entry whose resource has no launch recipe. Its fences (agent session id, runtime generation, action epoch) are unchanged. Plain shells fence on resource id and resource generation only.
- `TaskCockpitQuery::{Terminal, TerminalScroll, TerminalResize, TerminalReadiness}` gain `resource_id: Option<ResourceId>`. `None` keeps today's meaning: the provider terminal. Existing clients and Connect are unaffected.
- New `TaskCockpitQuery::TaskTerminals { task_id }` returns the strip: per terminal `resource_id`, `kind: Provider | Shell`, `title`, `label`, `runtime_state`, `live_cwd`, `exit`, `created_at`, `last_activity_at`; plus `order` and `focused`.
- The fenced terminal input bridge takes a `resource_id` instead of resolving "the task's terminal".
- Output draining and coalescing are per terminal with today's limits (64 chunks, 256 KiB pending, 64 retained deltas).

## 6. Client UX

The Terminal tab of the cockpit dock gains a strip along its top edge.

- One chip per terminal in durable order. Provider terminal first, labelled by provider. Shells show `title`, else `label`. A small dot marks a running child process.
- `+` chip opens a shell in the task's runtime directory. Chip menu: Rename, Open in project root, Close. Close confirms only when a child process is running.
- Selecting a chip swaps the grid and records focus via `SetTerminalStrip`. Drag reorders through the same command.
- Each shell has its own client-local viewport (scroll offset, selection) in its replica.
- Multi-task workspace rules hold: only the focused full pane owns terminal input; compact panes paint no terminals.
- An `Exited` shell stays greyed with its exit code until closed; its scrollback stays readable.
- Shortcuts: Ctrl+Shift+Backtick opens a shell; `Ctrl+Tab` / `Ctrl+Shift+Tab` cycle terminals while the terminal has focus. Routed through the existing `SendKeystroke` split.

## 7. Error handling

Errors are facts with named causes; none of them ends the host or another terminal.

| Condition | Behaviour |
| --- | --- |
| cwd missing or not a directory | `OpenShellTerminal` rejected naming the path; client offers the project root. |
| Shell program missing | Try candidates in order, record the one that launched; if none, reject listing the candidates tried. |
| Spawn or Job assignment failure | Existing suspended-launch cleanup; resource never becomes Active; error names the launcher stage. |
| Reader/wait actor failure | Governed by commits `39caf754` and `c99a0c5f`: only observed child exit ends a terminal; teardown misses are reported, never escalated. |
| Recipe decode failure | Reconciliation fault on the resource; store is not marked corrupt. |

## 8. Testing

All deterministic; no wall-clock gates.

- Domain: recipe round-trip; old wire shape decodes with defaults; unknown field still rejected; strip validation (duplicates, focused not in order, foreign resource); per-task cap.
- Kernel: each command through the command bus with fence rejection; `TerminalCwdReported` debounce and no-change suppression; `TerminalExited` recorded once; `TerminalActivity` coalescing.
- TerminalService: provider terminal plus two shells on one task have independent grids, sequences and drains; queries with `resource_id: None` return the provider terminal; `TaskTerminals` returns durable order; `Unknown` never returned outside the boot window.
- Process: real `pwsh` through the managed launcher reports cwd after `Set-Location` via the injected hook; PEB fallback agrees for `cmd`; label reflects a running child.
- UI: strip renders provider first, exited chip greyed, `+` dispatches with the runtime directory.
- Sabotage: remove the per-task cap and confirm the cap test fails; remove the debounce and confirm the cwd test fails.

## 9. Decisions log

- Approach A (extend the Terminal resource) chosen over a new `ResourceKind::Shell` and over a traycer-style dedicated record, to keep one identity scheme and let the restore lane enumerate `resources` directly.
- Live cwd is persisted (not only launch cwd), so restore lands in the last directory.
- Order and focus, exit state and timestamps are persisted.
- Provider terminal keeps its single-owner fences; the default of `resource_id: None` preserves every existing call site.
- Server-computed labels and shared process snapshots follow t3code and herdr; PowerShell prompt injection follows herdr.
