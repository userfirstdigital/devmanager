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
| Spawn or Job assignment failure | Existing suspended-launch cleanup. Section 4 registers the resource before it spawns, so the resource IS Active by then: the host records a `TerminalExited` fact whose summary is `spawn failed: <launcher stage>` (or `spawn refused: ...` when the shell never got as far as a launcher), and the resource is released through the ordinary close path. It does not stay half-open. |
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

Implementation decisions recorded on completion (2026-09-02):

- Host terminal facts (`terminal.cwd_reported`, `terminal.exited`, `terminal.activity`) are appended by `CommandBus::record_terminal_fact` with a NULL task revision, never as commands; `Command::OpenShellTerminal` is host-authority-only, so the client sends `NativeHostCommand::OpenShellTerminal { task_id, cwd }` and the host resolves cwd and shell and registers the resource.
- A provider terminal never carries a title, because the positional durable codec cannot represent `{ launch: None, title: Some }`; only plain shells are renamable.
- Closing the focused chip clears focus, and the terminal area then shows the random splash image (picsum) as in 0.4.1.
- Slice 1 reorders via the chip menu (Move left / Move right); drag reorder waits for an `on_drop` idiom in the GPUI shell.
- `MAX_PLAIN_SHELLS_PER_TASK` is enforced in `decide` (a clean rejection) and again in `apply_into` (a replay backstop); the strip is always a permutation of the task's non-`Released` plain shells.
- The kernel rebuilds the projection tables once when `open()` applied any migration (the V16 upgrade path); a transiently failed rebuild is not retried on the next open (deferred).
- Activity means real terminal output (the hosted sequence advanced), throttled to `TERMINAL_ACTIVITY_COALESCE_MS` (30 s); cwd is debounced by `TERMINAL_CWD_DEBOUNCE_MS` (2 s) and keeps the last sample when one is lost.
- Closing a shell is one owner: the host's `close_shell_terminal` retires the hosted view first, then the manager session, then removes the closed entry; the resource-release effect does not reach plain shells.
- Provider-resource selection has one rule, `domain::agent_resource::provider_terminal_resource`; `AgentResourceBinding::from_facts` refuses plain shells.
- The new resource-addressed terminal queries map to the read-only `terminal.view` action (`TaskCockpit`) locally, and viewport operations are fenced before mutation. On Connect the four viewport-mutating variants (`TerminalScroll`, `TerminalResize`, `TerminalScrollFor`, `TerminalResizeFor`) require `ActionId::MUTATE_TASK`, not `READ_TASK`: they move a host-owned PTY viewport and resize the pty itself however they are spelled on the wire. Only the screen, readiness and strip queries beside them are reads.
- `TaskTerminals.live_cwd` is redacted (workspace-relative, else the final component) because it crosses Connect. That redaction is narrower than it sounds and the honest statement is this: the shell's LAUNCH recipe is not redacted anywhere. `SnapshotItem::Resource(ResourceFacts)` carries `ResourceRecipe::Terminal { launch }` -- absolute cwd, program and args -- over `Query::SnapshotPage`, which Connect maps to `READ_TASK`, so any principal that can read the task snapshot can read the launch directory that `live_cwd` was redacted to hide. This is not new with plain shells (`ResourceRecipe::Service { command }` already crosses the same lane) and it is deliberately NOT changed in this wave. OPEN ITEM: redact resource recipes on the snapshot lane, which is the only thing that would make the `live_cwd` redaction mean what its name suggests.
- Chip labels in slice 1 are the launch program's file stem, computed by `terminal_label_for` in `src/host/cockpit.rs`. The server-computed running-child label and the running-child dot (sections 4.2 and 6) are DEFERRED to the port-ideas follow-up (item 20), together with the shared per-tick process snapshot they need; Task 8 addendum D deferred them and this is the entry that was missing. `kind: Provider | Shell` from section 5 is expressed on the wire as `is_provider` on the chip rather than as an enum.
- Environment facts measured on 2026-09-02: `powershell.exe` exits with `0xFFFF0000` about 3 s after a managed launch (pre-existing); `pwsh` 7 is an MSIX package resolved via PATH; Git Bash reports no cwd; `cmd.exe` reports cwd through the PEB rung only.
- The PowerShell prompt hook of §4.1 is applied to PLAIN SHELLS only, in `plain_shell_candidates`, because `shell_candidates` is shared with the provider terminal launch path and a provider owns its own shell contract. `pwsh_shell_args(shell_integration_enabled)` is the counterpart of the existing `bash_shell_args`; with shell integration off the arguments are `-NoLogo` exactly as before. The hook text is backslash-free (the OSC string terminator's second byte is spelled `[char]92`) so no quoting layer between the constant and `CreateProcess` can eat an escape, and it is pinned by a unit test because a typo in it produces a shell that starts perfectly and simply never reports a directory. `ShellSequenceParser` gained OSC 9;9 alongside OSC 7; the payload is a native path, so it is not percent-decoded, and both the quoted (ConEmu's documentation) and unquoted (this hook) spellings are accepted.
- Measured 2026-09-02 on PowerShell 7.6.5: without the hook `Set-Location` moves the PowerShell location and leaves `[Environment]::CurrentDirectory` at the launch directory, so neither rung of the cwd ladder ever moves; with the hook both do. Also measured: MSIX `pwsh` cannot be started through the managed launcher on this machine by either spelling on PATH (`C:\Program Files\WindowsApps\...\pwsh.exe` fails `CreateProcessW` with `0xC0070005` ACCESS_DENIED; the `AppData\Local\Microsoft\WindowsApps` execution alias fails to canonicalize with OS error 1920), with or without the hook, so that is pre-existing and unrelated to it. The managed-launcher live test therefore skips here with that error printed, and the hook is proved end to end instead by running it in a real `pwsh` child process and feeding the bytes it emits to the real parser. `DefaultTerminal::default()` is Bash, so a plain shell only reaches `pwsh` when a user selects it.
- Host-authority-only commands must be refused on both lanes: `CommandBus::execute` (defence in depth) and the wire-reachable `validate_authenticated_command_capability` journal-ingress group in `src/host/connection.rs`, because the live executor serves client envelopes through `execute_host_authorized`. `Command::OpenShellTerminal` is in both, and `connect/permissions.rs` denies its command form outright. This is the rule for any future host-only command.
- Cockpit `Denied` / `Unavailable` replies carry an additive optional `detail` string so a refusal names the offending path or the shells tried; the host still logs the same text.
- The client keys pending terminal state by `(HostTaskKey, TerminalTarget)` with `TerminalTarget { Provider, Resource(ResourceId) }` rather than a bare `ResourceId`: a start-pending retry armed by a legacy provider query has no resource id to key on, and a reserved nil id would conflate "the provider" with "this exact resource". Screens in the surface registry stay keyed by bare `ResourceId`, and the provider slot is resolved by searching for the non-shell projection, never by a fixed slot.
- A plain-shell projection from a host predating plain shells keys as the provider, so `TaskTerminalProjection::is_plain_shell` requires the nil session id AND a zero runtime generation, never `is_provider` alone.
- User ruling 2026-09-02: `focused: None` on the strip renders the splash photo whenever no shell chip is focused, including untouched tasks; the provider terminal is reached through its own chip. The strip's `order` holds plain shells only, so the wire cannot say "provider focused" and the client treats the provider chip as a local selection that clears strip focus.
- Focusing a chip also switches the center canvas to the Terminal view (user ruling 2026-09-02); the strip lives in the dock, the grid on the center canvas.
- A shell whose resource is `Releasing` renders as a muted "?" chip rather than as exited: the host's `TaskTerminals` synthesises `Unknown` for every id in `order` that has no hosted entry, so the spec's "in order, absent from terminals" state cannot arise from this host.
- The client disables the "+" at `MAX_PLAIN_SHELLS_PER_TASK` (the imported constant) and shows refused renames inline using `validate_terminal_title`; the host remains the authority for both rules.
