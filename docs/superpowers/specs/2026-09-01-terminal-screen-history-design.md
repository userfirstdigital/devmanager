# Terminal Screen History Design

**Date:** 2026-09-01
**Status:** Approved design, awaiting implementation plan
**Sub-project:** 3 of 3 (depends on `2026-09-01-shell-terminal-restore-design.md`)
**Related:** `2026-09-01-task-shell-terminals-design.md`, `2026-09-01-port-ideas-audit.md`

## 1. Problem

A shell restored after a host restart (sub-project 2) starts with a blank screen. The previous output, the context the user was working in, is gone. herdr solves this with an opt-in ANSI dump of each pane replayed into the fresh terminal. DevManager has no ANSI serializer, no styled export, and no pre-teardown hook in host shutdown.

This design captures each plain shell's screen and scrollback at graceful host exit and replays it above the restored shell's first prompt.

## 2. Goals and non-goals

Goals:

- On confirmed full quit and update handoff, write each plain shell's screen (scrollback plus viewport, styled) to a per-terminal sidecar file within a bounded shutdown budget.
- On restore, replay the file into the grid before the new shell starts, with a visible marker where the old session ended.
- On by default, with a setting that turns capture off and deletes existing files.

Non-goals:

- Capture on host crash or power loss (graceful exit only; the shell still restores in its cwd).
- Periodic snapshots.
- Provider terminals (their `--resume` repaint is their history).
- Cursor position, modes or alternate-screen state.

## 3. Capture

### 3.1 When

A new pre-abort step in the host's ordered shutdown (`finish_supervised_host`, before the executor is aborted) and in the update handoff path (before the recovery marker is persisted). Total budget 2 s; each terminal dump is attempted in turn and a dump that exceeds its share is skipped and logged, never awaited.

### 3.2 What

`TerminalSession::export_screen_history() -> Option<Vec<u8>>`: an ANSI serialization of the alacritty grid, scrollback plus viewport, produced by a new serializer that walks the same cells `snapshot_term` walks and emits SGR for foreground, background, bold, dim, italic, underline, inverse and strikethrough, resetting at line ends. Content only.

Returns `None` when the terminal is in the alternate screen (a `vim` or `htop` frame would replay as garbage) or when every cell is blank.

Cap: 1 MiB per terminal, trimmed from the top to a line boundary.

Why not the raw replay buffer or plain text: the 4 MiB raw buffer contains OSC and DSR sequences that would fire clipboard writes and device queries into the fresh shell; plain text loses colors.

### 3.3 Where

One sidecar file per terminal: `<profile_root>/terminals/<resource_id>.ansi`, written atomically (temp file in the same directory, then rename). Follows the existing sidecar precedents (provider session store, pids file). Nothing enters `kernel.sqlite3` except the pointer fact:

```rust
Event::TerminalHistoryCaptured { resource_id, bytes: u32, lines: u32, cols: u16, captured_at }
```

Files are deleted when their resource is released, after a successful replay, and when the setting is turned off.

### 3.4 Setting

`terminal.keep_screens_across_restarts: bool`, default `true`, exposed in Settings as "Keep terminal screens across restarts". Turning it off deletes `<profile_root>/terminals/` and disables capture; turning it on resumes capture at the next graceful exit. The setting is checked on both capture and replay.

## 4. Replay

`ensure_running(resource_id, seed)` (sub-project 2) receives the seed when the setting is on, the file exists, and the resource has no live runtime.

1. Feed the ANSI bytes into the fresh alacritty grid through the normal parser **before** the PTY child is spawned. The shell's first prompt lands below the replayed content and the old screen scrolls into scrollback; interleaving is impossible by construction.
2. Append one dim marker line after the replayed content: `── restored <local time> ──`.
3. Before the first live PTY byte, drain everything the replay triggered in the parser: pending clipboard writes, bell, title changes, cwd reports. Replayed content never emits a live event and never produces a durable `TerminalCwdReported`.
4. Spawn at the recorded cols and rows (sub-project 2), which match the capture width, so no reflow occurs at replay time.
5. Delete the file after a successful replay, so a screen is restored at most once. A file that fails to parse or exceeds the cap is deleted and logged; the shell restores clean.

## 5. Error handling

| Condition | Behaviour |
| --- | --- |
| Serializer error, disk full, per-terminal timeout | Logged per terminal; skipped; shutdown continues within budget. |
| Missing file at restore | Normal case; clean restore. |
| Unparseable or oversized file | Deleted with a log line; clean restore. |
| Setting off at restore | Files ignored and deleted. |
| Setting toggled during shutdown | The value read at shutdown start wins. |

## 6. Testing

- Serializer: round-trip a grid with colors, attributes, wide characters and scrollback through serialize → parse into a fresh grid → compare cell snapshots; alternate screen returns `None`; blank returns `None`; cap trims from the top at a line boundary.
- Capture step: shutdown with three live shells writes three files atomically within budget; one deliberately slow terminal is skipped and logged; provider terminals produce no file.
- Replay: seeded content precedes the first prompt in the grid; a dump containing OSC 52 and OSC 7 produces no clipboard write and no `TerminalCwdReported`; the marker line appears once; the file is deleted after replay.
- Setting: off deletes the directory and disables capture; on resumes.
- End to end: open a shell, print colored output, full-quit the host, start it, attach a client, assert the colored lines appear above the new prompt with the marker between them.
- Sabotage: remove the alternate-screen guard and confirm the vim-frame test fails; remove the side-effect drain and confirm the OSC 52 test fails.

## 7. Decisions log

- Graceful-exit capture only; no periodic snapshots.
- On by default with an off switch that deletes, given the profile directory already holds conversation history at the same trust level.
- Styled ANSI via a new serializer, not raw bytes and not plain text.
- 1 MiB cap per terminal.
- Seed before spawn, drain side effects, marker line, delete after replay (herdr's ordering and drain; t3code's detach-during-restore concern addressed by seeding before the PTY exists).
