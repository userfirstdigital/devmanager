# Terminal Screen History Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Capture each plain shell's styled screen and scrollback at graceful host exit into a per-terminal sidecar file, and replay it above the restored shell's first prompt, behind an on-by-default setting.

**Architecture:** A new ANSI serializer walks the alacritty grid and emits SGR-styled lines; a pre-abort shutdown step writes one `<profile_root>/terminals/<resource_id>.ansi` per live plain shell within a 2 s budget and records a `terminal.history_captured` fact. `ensure_running` (plan 2) feeds the file into the fresh grid before the PTY child spawns, appends a marker line, drains replay side effects before the first live byte, and deletes the file.

**Tech Stack:** Rust 2021, alacritty_terminal grid types, existing sidecar-file conventions, existing settings plumbing.

**Spec:** `docs/superpowers/specs/2026-09-01-terminal-screen-history-design.md`
**Depends on:** plans `2026-09-01-task-shell-terminals.md` and `2026-09-01-shell-terminal-restore.md` landed.

## Global Constraints

- Isolated `CARGO_TARGET_DIR` under `C:\Temp\devmanager-*`; focused tests while iterating; `cargo check --locked --lib --bins --tests` before handing back.
- Capture only at graceful exit (confirmed full quit and update handoff), total budget 2 s, per-terminal share `2 s / live plain shells`, minimum 100 ms.
- Serializer output is content only: no cursor, no modes. Alternate screen or blank screen → `None`.
- Cap 1 MiB per terminal, trimmed from the top at a line boundary.
- Files: `<profile_root>/terminals/<resource_id>.ansi`, atomic temp + rename, deleted on resource release, after successful replay, and when the setting is turned off.
- Setting `terminal.keep_screens_across_restarts`, default `true`, JSON name `keepTerminalScreensAcrossRestarts`.
- Provider terminals are never captured or replayed.
- Seed before spawn; drain clipboard/bell/title/cwd side effects before the first live PTY byte; the marker line is `── restored <local time> ──`.

## File map

- Create `src/terminal/ansi_export.rs` (serializer + cap)
- Modify `src/terminal/session.rs` (`export_screen_history`, `seed_history_before_prompt`, side-effect drain)
- Create `src/host/terminal_history.rs` (sidecar store: path, write, read, delete)
- Modify `src/config/model.rs`, `src/models/config.rs`, `src/workspace/mod.rs`, `src/app/mod.rs`, `src/ui/native_shell.rs` (setting)
- Modify `src/domain/event.rs`, `src/domain/command.rs`, `src/kernel/projector.rs` (`TerminalHistoryCaptured`, host-only command)
- Modify `src/bin/devmanager-host.rs`, `src/updater/mod.rs` (capture hook), `src/host/connection.rs` (capture step, replay in `ensure_running`, delete on release / setting off)
- Tests: unit tests beside each file; `tests/screen_history.rs`

---

### Task 1: ANSI serializer

**Files:**
- Create: `src/terminal/ansi_export.rs`; add `pub mod ansi_export;` to `src/terminal/mod.rs`
- Test: same file

**Interfaces:**
- Produces:

```rust
pub const MAX_HISTORY_BYTES: usize = 1024 * 1024;
pub fn export_screen_ansi(snapshot: &TerminalScreenSnapshot) -> Option<Vec<u8>>;   // None on alt-screen or blank
pub fn cap_history_from_top(bytes: Vec<u8>, max: usize) -> Vec<u8>;              // trims to a line boundary
```
- Uses `TerminalScreenSnapshot { lines: Vec<Vec<TerminalCellSnapshot>>, mode: TerminalModeSnapshot { alternate_screen, .. }, .. }` from `src/terminal/session.rs:156-245`. `lines` covers scrollback plus viewport (`snapshot_term` builds `total_lines` rows).

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::session::{TerminalCellSnapshot, TerminalModeSnapshot, TerminalScreenSnapshot};

    fn cell(ch: char, fg: u32, bg: u32, bold: bool) -> TerminalCellSnapshot {
        TerminalCellSnapshot {
            character: ch, zero_width: Vec::new(), foreground: fg, background: bg,
            bold, dim: false, italic: false, underline: false, undercurl: false, strike: false,
            hidden: false, has_hyperlink: false, default_background: bg == 0, default_foreground: fg == 0xFFFFFF,
        }
    }

    fn snapshot(lines: Vec<Vec<TerminalCellSnapshot>>, alternate: bool) -> TerminalScreenSnapshot {
        let rows = lines.len();
        let cols = lines.first().map(|l| l.len()).unwrap_or(0);
        TerminalScreenSnapshot {
            cells: Vec::new(), lines, cursor: None, display_offset: 0, history_size: 0,
            total_lines: rows, rows, cols,
            mode: TerminalModeSnapshot { alternate_screen: alternate, ..Default::default() },
        }
    }

    #[test]
    fn styled_lines_round_trip_through_a_fresh_grid() {
        let red_bold = cell('E', 0xFF0000, 0, true);
        let plain = cell('x', 0xFFFFFF, 0, false);
        let lines = vec![
            vec![red_bold.clone(), red_bold.clone(), plain.clone()],
            vec![plain.clone(), cell(' ', 0xFFFFFF, 0, false), cell(' ', 0xFFFFFF, 0, false)],
        ];
        let ansi = export_screen_ansi(&snapshot(lines, false)).expect("non-blank exports");
        let text = String::from_utf8(ansi.clone()).expect("utf8");
        assert!(text.contains("\u{1b}[1m"), "bold SGR present: {text:?}");
        assert!(text.contains("\u{1b}[38;2;255;0;0m"), "truecolor red present: {text:?}");
        assert!(text.ends_with("\u{1b}[0m\r\n"), "each line resets and ends with CRLF: {text:?}");

        // Parse back into a fresh alacritty grid and compare characters and bold.
        let replica = crate::terminal::session::TerminalReplica::from_bootstrap("ansi-test", Default::default(), &ansi);
        let parsed = replica.snapshot();
        assert_eq!(parsed.lines[0][0].character, 'E');
        assert!(parsed.lines[0][0].bold);
        assert_eq!(parsed.lines[0][0].foreground, 0xFF0000);
        assert_eq!(parsed.lines[1][0].character, 'x');
    }

    #[test]
    fn alternate_screen_and_blank_export_nothing() {
        let plain = cell(' ', 0xFFFFFF, 0, false);
        assert!(export_screen_ansi(&snapshot(vec![vec![plain.clone(); 4]], false)).is_none());
        let letter = cell('a', 0xFFFFFF, 0, false);
        assert!(export_screen_ansi(&snapshot(vec![vec![letter]], true)).is_none());
    }

    #[test]
    fn cap_trims_from_the_top_at_a_line_boundary() {
        let mut bytes = Vec::new();
        for i in 0..100 {
            bytes.extend_from_slice(format!("line {i:03}\r\n").as_bytes());
        }
        let capped = cap_history_from_top(bytes.clone(), 200);
        assert!(capped.len() <= 200);
        assert!(capped.starts_with(b"line "), "must start at a line boundary: {:?}", String::from_utf8_lossy(&capped));
        assert!(capped.ends_with(b"line 099\r\n"));
        assert_eq!(cap_history_from_top(bytes.clone(), usize::MAX), bytes);
    }
}
```

Check `TerminalReplica::from_bootstrap`'s exact runtime argument type (`SessionRuntimeState`) and construct it with `Default::default()` if it implements `Default`, otherwise with the fixture the existing replica tests use.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib --locked terminal::ansi_export -- --test-threads=1`
Expected: compile error (module missing).

- [ ] **Step 3: Implement**

```rust
//! Styled ANSI export of a terminal screen snapshot. Content only: no cursor,
//! no modes. Used for screen history across host restarts.

use crate::terminal::session::{TerminalCellSnapshot, TerminalScreenSnapshot};

pub const MAX_HISTORY_BYTES: usize = 1024 * 1024;

pub fn export_screen_ansi(snapshot: &TerminalScreenSnapshot) -> Option<Vec<u8>> {
    if snapshot.mode.alternate_screen {
        return None;
    }
    let last_non_blank = snapshot.lines.iter().rposition(|line| line.iter().any(|c| !is_blank(c)))?;
    let mut out = Vec::with_capacity(snapshot.lines.len() * (snapshot.cols + 16));
    for line in &snapshot.lines[..=last_non_blank] {
        let trimmed_len = line.iter().rposition(|c| !is_blank(c)).map(|i| i + 1).unwrap_or(0);
        let mut style = Style::default();
        for cell in &line[..trimmed_len] {
            let wanted = Style::from_cell(cell);
            if wanted != style {
                wanted.emit_transition(&mut out);
                style = wanted;
            }
            let mut buf = [0u8; 4];
            out.extend_from_slice(cell.character.encode_utf8(&mut buf).as_bytes());
            for zw in &cell.zero_width {
                out.extend_from_slice(zw.encode_utf8(&mut buf).as_bytes());
            }
        }
        out.extend_from_slice(b"\x1b[0m\r\n");
    }
    Some(out)
}

fn is_blank(cell: &TerminalCellSnapshot) -> bool {
    (cell.character == ' ' || cell.character == '\0') && cell.zero_width.is_empty() && cell.default_background
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Style {
    fg: Option<u32>,
    bg: Option<u32>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

impl Style {
    fn from_cell(cell: &TerminalCellSnapshot) -> Self {
        Self {
            fg: (!cell.default_foreground).then_some(cell.foreground),
            bg: (!cell.default_background).then_some(cell.background),
            bold: cell.bold,
            dim: cell.dim,
            italic: cell.italic,
            underline: cell.underline || cell.undercurl,
            strike: cell.strike,
        }
    }

    /// Emit a full reset followed by this style. Simple and always correct;
    /// the cap bounds the output, not the byte-efficiency of transitions.
    fn emit_transition(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[0m");
        if self.bold { out.extend_from_slice(b"\x1b[1m"); }
        if self.dim { out.extend_from_slice(b"\x1b[2m"); }
        if self.italic { out.extend_from_slice(b"\x1b[3m"); }
        if self.underline { out.extend_from_slice(b"\x1b[4m"); }
        if self.strike { out.extend_from_slice(b"\x1b[9m"); }
        if let Some(fg) = self.fg {
            out.extend_from_slice(format!("\x1b[38;2;{};{};{}m", (fg >> 16) & 0xFF, (fg >> 8) & 0xFF, fg & 0xFF).as_bytes());
        }
        if let Some(bg) = self.bg {
            out.extend_from_slice(format!("\x1b[48;2;{};{};{}m", (bg >> 16) & 0xFF, (bg >> 8) & 0xFF, bg & 0xFF).as_bytes());
        }
    }
}

pub fn cap_history_from_top(bytes: Vec<u8>, max: usize) -> Vec<u8> {
    if bytes.len() <= max {
        return bytes;
    }
    let start = bytes.len() - max;
    let boundary = bytes[start..].iter().position(|b| *b == b'\n').map(|i| start + i + 1).unwrap_or(bytes.len());
    bytes[boundary..].to_vec()
}
```

Confirm the foreground/background `u32` layout in `resolve_terminal_color` (`session.rs:3624`) is `0xRRGGBB`; if it is `0xAARRGGBB`, mask with `& 0xFF_FFFF` in `from_cell`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib --locked terminal::ansi_export -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/terminal/ansi_export.rs src/terminal/mod.rs
git commit -m "feat(terminal): styled ANSI export of screen snapshots with a top-trimming cap

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 2: Session export and pre-spawn seeding with side-effect drain

**Files:**
- Modify: `src/terminal/session.rs` (`TerminalSession::export_screen_history`, `seed_history_before_prompt`, drain flag in the reader), `src/services/process_manager.rs` (`spawn_task_shell_session` gains `seed: Option<&[u8]>`)
- Test: `src/terminal/session.rs` tests

**Interfaces:**
- `TerminalSession::export_screen_history(&self) -> Option<Vec<u8>>` = `export_screen_ansi(&self.snapshot())` then `cap_history_from_top(.., MAX_HISTORY_BYTES)`.
- Seeding happens inside `spawn_with_command` between `Term::new` (`session.rs:2990-2994`) and the PTY spawn: a new parameter `seed: Option<&[u8]>` on `spawn_with_command`, `spawn_command`, and `spawn_task_shell_session`. When present: `parser.advance(&mut term, seed)`, then `parser.advance(&mut term, marker_line().as_bytes())`, then set `session.replay_side_effects_pending = true`.
- Reader drain: in `spawn_reader_thread`, before applying the first live chunk, if `replay_side_effects_pending` is set: clear pending clipboard writes, bell count, title change, and the shell-sequence parser's pending cwd report from the seeded bytes; then clear the flag. Concretely `ShellSequenceParser::default()` is created fresh for the reader, so cwd reports from the seed never reach it; the term-level events to discard are those `SessionEventProxy` would forward (clipboard store, bell, title): guard `SessionEventProxy` handlers with `if self.replaying.load(Ordering::Acquire) { return; }` and set `replaying = true` during seeding, `false` after.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn seeded_history_precedes_the_first_live_line_and_emits_no_side_effects() {
        // Build a TerminalSession over a fake PTY exactly as the existing spawn-path tests do
        // (grep "fn spawn_fake_session" / the fixture used by `production_terminal_spawn_uses_suspended_managed_launch`);
        // if none is process-free, use TerminalReplica::from_bootstrap for the grid half and a unit test on
        // SessionEventProxy for the side-effect half.
        let seed = b"\x1b[1mold line\x1b[0m\r\n\x1b]52;c;aGVsbG8=\x07\x1b]7;file:///C:/elsewhere\x07";
        let (session, clipboard_writes, bells) = fake_session_with_seed(Some(seed));
        let snapshot = session.snapshot();
        assert_eq!(text_of(&snapshot.lines[0]), "old line");
        assert!(text_of(&snapshot.lines[1]).contains("restored"), "marker line follows seed");
        assert_eq!(clipboard_writes.load(Ordering::Acquire), 0, "OSC 52 in the seed must not write the clipboard");
        assert_eq!(bells.load(Ordering::Acquire), 0);
        assert_eq!(session.runtime_cwd(), None, "OSC 7 in the seed must not report a cwd");
        session.feed_live(b"\x07live\r\n");
        assert_eq!(bells.load(Ordering::Acquire), 1, "live bells still surface");
    }

    #[test]
    fn export_screen_history_is_none_for_alternate_screen() {
        let (session, _, _) = fake_session_with_seed(None);
        session.feed_live(b"\x1b[?1049h");
        assert!(session.export_screen_history().is_none());
        session.feed_live(b"\x1b[?1049l");
        session.feed_live(b"hello\r\n");
        assert!(session.export_screen_history().is_some());
    }
```

Write `fake_session_with_seed`, `text_of`, `feed_live`, and `runtime_cwd` as test-module helpers over the existing process-free construction path (`TerminalSession` fields are private; the helpers live in `session.rs`'s test module, which already constructs `SessionEventProxy` directly at the `from_bootstrap` site).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib --locked terminal::session::tests::seeded_history -- --test-threads=1`
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
    pub fn export_screen_history(&self) -> Option<Vec<u8>> {
        let ansi = crate::terminal::ansi_export::export_screen_ansi(&self.snapshot())?;
        Some(crate::terminal::ansi_export::cap_history_from_top(ansi, crate::terminal::ansi_export::MAX_HISTORY_BYTES))
    }

fn history_marker_line() -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M");
    format!("\x1b[2m\u{2500}\u{2500} restored {now} \u{2500}\u{2500}\x1b[0m\r\n")
}
```

(If `chrono` is not a dependency, use the repo's existing local-time formatter; grep `format_local_time` in `src/`.)

In `spawn_with_command`, after `Term::new`:

```rust
    if let Some(seed) = seed {
        event_proxy.replaying.store(true, Ordering::Release);
        {
            let mut parser = Processor::<StdSyncHandler>::new();
            let mut term = term.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            parser.advance(&mut *term, seed);
            parser.advance(&mut *term, history_marker_line().as_bytes());
        }
        event_proxy.replaying.store(false, Ordering::Release);
    }
```

Add `replaying: Arc<AtomicBool>` to `SessionEventProxy` (default false; every construction site sets it) and early-return in its clipboard-store, bell, and title handlers while `replaying` is true. Thread `seed: Option<&[u8]>` through `spawn_command` → `spawn_with_command`, and through `ProcessManager::spawn_task_shell_session(.., seed: Option<&[u8]>)`; every existing caller passes `None`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib --locked terminal::session -- --test-threads=1`; `cargo check --locked --lib --bins --tests`
Expected: PASS; EXIT 0.

- [ ] **Step 5: Commit**

```bash
git add src/terminal/session.rs src/services/process_manager.rs $(git diff --name-only -- src)
git commit -m "feat(terminal): export screen history and seed it before the PTY spawns

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 3: Sidecar store, setting, and captured fact

**Files:**
- Create: `src/host/terminal_history.rs`; `pub(crate) mod terminal_history;` in `src/host/mod.rs`
- Modify: `src/config/model.rs` (`Settings`, `SettingsWire`, `SETTINGS_FIELDS`, `SettingsField`, `json_name`, patch merge, setter), `src/models/config.rs` (`Settings` runtime field + default), `src/domain/event.rs`, `src/domain/command.rs`, `src/kernel/projector.rs`
- Test: `src/host/terminal_history.rs` tests, `src/config/model.rs` tests

**Interfaces:**
- Produces:

```rust
pub struct TerminalHistoryStore { root: PathBuf }   // <profile_root>/terminals
impl TerminalHistoryStore {
    pub fn new(profile_root: &Path) -> Self;
    pub fn path_for(&self, resource_id: ResourceId) -> PathBuf;            // <root>/<resource_id>.ansi
    pub fn write(&self, resource_id: ResourceId, bytes: &[u8]) -> std::io::Result<()>;  // temp + rename
    pub fn read(&self, resource_id: ResourceId) -> std::io::Result<Option<Vec<u8>>>;    // None if absent; Err if oversized
    pub fn delete(&self, resource_id: ResourceId) -> std::io::Result<()>;
    pub fn delete_all(&self) -> std::io::Result<()>;
}
```
- Setting: `Settings.keep_terminal_screens_across_restarts: bool` default `true`; `SettingsField::KeepTerminalScreensAcrossRestarts`; JSON `keepTerminalScreensAcrossRestarts`; setter `set_keep_terminal_screens_across_restarts`.
- Event `TerminalHistoryCaptured { resource_id, bytes: u32, lines: u32, cols: u16 }`, wire `terminal.history_captured`, host fact (no revision), `apply_into` records `facts.history_captured_at_ms = occurred_at_ms` (new `Option<i64>` field on `TerminalFacts` + column `history_captured_at_ms INTEGER` in the terminal_facts table via V17 `ALTER TABLE terminal_facts ADD COLUMN history_captured_at_ms INTEGER`).
- Host-only `Command::RecordTerminalHistoryCaptured { resource_id, bytes, lines, cols }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ResourceId;

    #[test]
    fn store_writes_reads_and_deletes_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let store = TerminalHistoryStore::new(dir.path());
        let id = ResourceId::new();
        assert_eq!(store.read(id).unwrap(), None);
        store.write(id, b"hello\r\n").unwrap();
        assert_eq!(store.path_for(id).extension().and_then(|e| e.to_str()), Some("ansi"));
        assert!(!dir.path().join("terminals").read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
        assert_eq!(store.read(id).unwrap(), Some(b"hello\r\n".to_vec()));
        store.delete(id).unwrap();
        assert_eq!(store.read(id).unwrap(), None);
        store.write(id, b"x").unwrap();
        store.delete_all().unwrap();
        assert!(!dir.path().join("terminals").exists());
    }

    #[test]
    fn oversized_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = TerminalHistoryStore::new(dir.path());
        let id = ResourceId::new();
        std::fs::create_dir_all(dir.path().join("terminals")).unwrap();
        std::fs::write(store.path_for(id), vec![b'a'; crate::terminal::ansi_export::MAX_HISTORY_BYTES + 1]).unwrap();
        assert!(store.read(id).is_err());
    }
}
```

`src/config/model.rs` test (copy the shape of the existing `show_terminal_scrollbar` round-trip test):

```rust
    #[test]
    fn keep_terminal_screens_setting_defaults_true_and_round_trips() {
        let settings = Settings::default();
        assert!(settings.keep_terminal_screens_across_restarts);
        let json = serde_json::to_value(&settings.to_wire()).unwrap();
        assert_eq!(json["keepTerminalScreensAcrossRestarts"], serde_json::Value::Bool(true));
        assert!(SETTINGS_FIELDS.contains(&"keepTerminalScreensAcrossRestarts"));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib --locked host::terminal_history config::model::tests::keep_terminal -- --test-threads=1`
Expected: compile errors.

- [ ] **Step 3: Implement**

```rust
//! Per-terminal screen history sidecar files under <profile_root>/terminals.

use std::path::{Path, PathBuf};

use crate::domain::ResourceId;
use crate::terminal::ansi_export::MAX_HISTORY_BYTES;

pub struct TerminalHistoryStore {
    root: PathBuf,
}

impl TerminalHistoryStore {
    pub fn new(profile_root: &Path) -> Self {
        Self { root: profile_root.join("terminals") }
    }

    pub fn path_for(&self, resource_id: ResourceId) -> PathBuf {
        self.root.join(format!("{resource_id}.ansi"))
    }

    pub fn write(&self, resource_id: ResourceId, bytes: &[u8]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let target = self.path_for(resource_id);
        let temp = self.root.join(format!("{resource_id}.{}.tmp", std::process::id()));
        std::fs::write(&temp, bytes)?;
        match std::fs::rename(&temp, &target) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = std::fs::remove_file(&temp);
                Err(error)
            }
        }
    }

    pub fn read(&self, resource_id: ResourceId) -> std::io::Result<Option<Vec<u8>>> {
        let path = self.path_for(resource_id);
        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if metadata.len() > MAX_HISTORY_BYTES as u64 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "terminal history exceeds cap"));
        }
        std::fs::read(&path).map(Some)
    }

    pub fn delete(&self, resource_id: ResourceId) -> std::io::Result<()> {
        match std::fs::remove_file(self.path_for(resource_id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn delete_all(&self) -> std::io::Result<()> {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}
```

Setting: follow the twelve-step `show_terminal_scrollbar` chain listed in the extraction notes (`src/models/config.rs:135,160`; `src/config/model.rs:605,633,667,701,822,834+,773-775,959-963,1720,1757,3155`; `src/persistence/mod.rs:563,761`; `src/workspace/mod.rs:1248,4392,2058-2063`; `src/app/mod.rs:11201-11207,8845,10118`) with the new name. In the GPUI shell, add a labelled ghost-button row "Keep terminal screens across restarts · On/Off" in `render_appearance_settings_content` (idiom at `native_shell.rs:33564-33583`) that toggles the field through the config store and, when turning off, dispatches `ActionRequest::TerminalHistoryClear` (new, host-only handling: `TerminalHistoryStore::delete_all()`).

Event/command/projector: add `TerminalHistoryCaptured` exactly like `TerminalActivity` in plan 1 Task 3 (host fact, no revision), plus `history_captured_at_ms: Option<i64>` on `TerminalFacts`, the V17 column, projector `UPDATE terminal_facts SET history_captured_at_ms = ?1 WHERE resource_id = ?2`, loader column, and `Command::RecordTerminalHistoryCaptured` in the host-only list.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib --locked host::terminal_history config::model domain::event kernel:: -- --test-threads=1`; `cargo check --locked --lib --bins --tests`
Expected: PASS; EXIT 0.

- [ ] **Step 5: Commit**

```bash
git add src/host/terminal_history.rs src/host/mod.rs src/config/model.rs src/models/config.rs src/workspace/mod.rs src/app/mod.rs src/ui/native_shell.rs src/persistence/mod.rs src/domain/event.rs src/domain/command.rs src/domain/terminal_facts.rs src/kernel/projector.rs src/kernel/schema.rs src/kernel/command_bus.rs
git commit -m "feat(host): terminal history sidecar store, setting, and captured fact

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 4: Capture at graceful exit

**Files:**
- Modify: `src/host/connection.rs` (`capture_terminal_histories(&mut self, budget: Duration) -> CaptureReport`), `src/bin/devmanager-host.rs:996-1079` (`finish_supervised_host` pre-abort call), `src/updater/mod.rs:885-901` (before `persist_update_handoff_recovery_marker`)
- Test: `src/host/connection.rs` unit test with fixture terminals

**Interfaces:**
- `pub struct CaptureReport { pub written: Vec<ResourceId>, pub skipped: Vec<(ResourceId, String)> }`
- `HostRequestExecutor::capture_terminal_histories(&mut self, budget: Duration) -> CaptureReport`: reads the setting; iterates `shell_sessions` (plain shells only); per terminal, with share `budget / n` (min 100 ms): `export_screen_history()` on the live session, `TerminalHistoryStore::write`, `RecordTerminalHistoryCaptured`; skips and logs on `None`, error, or elapsed share.
- Request path: `ExecutorControl::CaptureTerminalHistories { budget, ack: oneshot::Sender<CaptureReport> }` so `finish_supervised_host` can ask through `request_handle` before `executor_task.abort()`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn capture_writes_one_file_per_live_shell_within_budget_and_skips_alt_screen() {
        let mut harness = executor_for_test_with_fake_shells(3);   // three fixture shells with text; the second in alt-screen
        harness.set_setting_keep_screens(true);
        let report = harness.executor.capture_terminal_histories(Duration::from_secs(2));
        assert_eq!(report.written.len(), 2);
        assert_eq!(report.skipped.len(), 1);
        for id in &report.written {
            assert!(harness.history_store().read(*id).unwrap().is_some());
        }
        let snapshot = harness.executor.bus.task_snapshot(harness.task_id).unwrap().unwrap();
        assert!(report.written.iter().all(|id| snapshot.terminal_facts[id].history_captured_at_ms.is_some()));
    }

    #[test]
    fn capture_is_a_no_op_when_the_setting_is_off_and_ignores_provider_terminals() {
        let mut harness = executor_for_test_with_fake_shells(1);
        harness.set_setting_keep_screens(false);
        let report = harness.executor.capture_terminal_histories(Duration::from_secs(2));
        assert!(report.written.is_empty());
    }
```

Build the harness on the existing executor test fixture and `TerminalReplica`-backed fixture runtimes (`ProjectionSource::Fixture`), registering them in `shell_sessions` with a fake `ShellSessionLink`; expose `export_screen_history` on the fixture through the `AttachedTerminalRuntime` trait (add `fn export_screen_history(&self) -> Option<Vec<u8>>` with a default `None` and implement it for `TerminalSession` and the fixture).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib --locked host::connection::tests::capture_ -- --test-threads=1`
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
    pub fn capture_terminal_histories(&mut self, budget: Duration) -> CaptureReport {
        let mut report = CaptureReport::default();
        if !self.keep_terminal_screens_setting() {
            return report;
        }
        let store = crate::host::terminal_history::TerminalHistoryStore::new(&self.profile_root);
        let targets = self.shell_sessions.iter().map(|(id, link)| (*id, link.task_id)).collect::<Vec<_>>();
        if targets.is_empty() {
            return report;
        }
        let share = std::cmp::max(budget / targets.len() as u32, Duration::from_millis(100));
        let started = Instant::now();
        for (resource_id, task_id) in targets {
            if started.elapsed() >= budget {
                report.skipped.push((resource_id, "shutdown budget exhausted".to_string()));
                continue;
            }
            let per_terminal = Instant::now();
            let export = self.terminal_service.export_screen_history(resource_id);
            let bytes = match export {
                Ok(Some(bytes)) => bytes,
                Ok(None) => { report.skipped.push((resource_id, "alternate screen or blank".to_string())); continue; }
                Err(error) => { report.skipped.push((resource_id, format!("export failed: {error}"))); continue; }
            };
            if per_terminal.elapsed() > share {
                report.skipped.push((resource_id, "per-terminal budget exceeded".to_string()));
                continue;
            }
            if let Err(error) = store.write(resource_id, &bytes) {
                report.skipped.push((resource_id, format!("write failed: {error}")));
                continue;
            }
            let lines = bytes.iter().filter(|b| **b == b'\n').count() as u32;
            let cols = self.terminal_service.terminal_cols(resource_id).unwrap_or(0);
            let _ = self.execute_host_fact(task_id, Command::RecordTerminalHistoryCaptured {
                resource_id, bytes: bytes.len() as u32, lines, cols,
            });
            report.written.push(resource_id);
        }
        for (resource_id, reason) in &report.skipped {
            eprintln!("devmanager-host: terminal history for {resource_id} skipped: {reason}");
        }
        report
    }
```

`TerminalService::export_screen_history(resource_id) -> Result<Option<Vec<u8>>, TerminalError>` locates the hosted terminal by `resource_id` and calls the runtime's `export_screen_history()`; `terminal_cols(resource_id)` returns `spec.size.cols`. `profile_root` is the `ResolvedAppPaths.root` the executor already receives (add the field if it only has `database`).

Control message: add `ExecutorControl::CaptureTerminalHistories { budget: Duration, ack: tokio::sync::oneshot::Sender<CaptureReport> }` handled in `handle_control` by calling the method and sending the report. Add `HostRequestHandle::capture_terminal_histories(&self, budget) -> Result<CaptureReport, String>` that sends the control and awaits the ack with a `budget + 500ms` timeout.

In `finish_supervised_host` (`devmanager-host.rs`), for `intentional_match.is_some()` (confirmed full quit) call `request_handle.capture_terminal_histories(Duration::from_secs(2)).await` before `drain_then_abort_connection_tasks`, logging the counts. In the updater, before `persist_update_handoff_recovery_marker` (`updater/mod.rs:890`), call the same through the host request handle the updater already holds for drain (`self.inner.request_handle`; if the updater has no handle, add the call to the host's `ArmUpdateInstall` handler instead, which runs on the executor and can call `capture_terminal_histories` directly).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib --locked host::connection::tests::capture_ -- --test-threads=1`; `cargo check --locked --lib --bins --tests`
Expected: PASS; EXIT 0.

- [ ] **Step 5: Commit**

```bash
git add src/host/connection.rs src/terminal/service.rs src/terminal/session.rs src/bin/devmanager-host.rs src/updater/mod.rs
git commit -m "feat(host): capture plain shell screens at graceful exit within a bounded budget

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 5: Replay in `ensure_running`, deletion, setting-off cleanup

**Files:**
- Modify: `src/host/connection.rs` (`ensure_running`, release path, `TerminalHistoryClear` handling)
- Test: `src/host/connection.rs` unit tests

**Interfaces:**
- `ensure_running(resource_id, seed)` (plan 2) now computes `seed` itself when the caller passes `None`: `if setting_on { store.read(resource_id) }`; on `Err` (oversized/unreadable) delete the file and log; on `Ok(Some(bytes))` pass to `spawn_task_shell_session(.., Some(&bytes))` and delete the file after the spawn succeeds.
- On `ResourceReleased` for a Terminal, `store.delete(resource_id)`.
- `ActionRequest::TerminalHistoryClear` → executor `store.delete_all()`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn ensure_running_seeds_from_the_store_and_deletes_the_file() {
        let mut harness = executor_for_test_with_spawn_recorder();   // records the seed passed to spawn_task_shell_session without spawning
        harness.set_setting_keep_screens(true);
        let shell = harness.open_plain_shell_durably();               // resource exists, no live runtime
        harness.history_store().write(shell, b"old\r\n").unwrap();
        harness.executor.ensure_running(shell, None).unwrap();
        assert_eq!(harness.recorded_seed(shell), Some(b"old\r\n".to_vec()));
        assert_eq!(harness.history_store().read(shell).unwrap(), None, "deleted after successful replay");
    }

    #[test]
    fn ensure_running_ignores_and_deletes_history_when_setting_is_off() {
        let mut harness = executor_for_test_with_spawn_recorder();
        harness.set_setting_keep_screens(false);
        let shell = harness.open_plain_shell_durably();
        harness.history_store().write(shell, b"old\r\n").unwrap();
        harness.executor.ensure_running(shell, None).unwrap();
        assert_eq!(harness.recorded_seed(shell), None);
        assert_eq!(harness.history_store().read(shell).unwrap(), None);
    }

    #[test]
    fn oversized_history_is_deleted_and_shell_restores_clean() {
        let mut harness = executor_for_test_with_spawn_recorder();
        harness.set_setting_keep_screens(true);
        let shell = harness.open_plain_shell_durably();
        std::fs::create_dir_all(harness.history_store().path_for(shell).parent().unwrap()).unwrap();
        std::fs::write(harness.history_store().path_for(shell), vec![b'a'; MAX_HISTORY_BYTES + 1]).unwrap();
        harness.executor.ensure_running(shell, None).unwrap();
        assert_eq!(harness.recorded_seed(shell), None);
        assert_eq!(harness.history_store().read(shell).unwrap(), None);
    }
```

The spawn recorder is a test-only hook on `ConfiguredServiceRuntime`/`ProcessManager` (`#[cfg(test)] spawn_recorder: Option<Arc<Mutex<HashMap<ResourceId, Option<Vec<u8>>>>>>`) consulted by `spawn_task_shell_session` before spawning; when set it records and returns a fixture session id without spawning.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib --locked host::connection::tests::ensure_running_seeds -- --test-threads=1`
Expected: compile error.

- [ ] **Step 3: Implement**

In `ensure_running`, before spawning:

```rust
        let store = crate::host::terminal_history::TerminalHistoryStore::new(&self.profile_root);
        let seed = match seed {
            Some(seed) => Some(seed),
            None if self.keep_terminal_screens_setting() => match store.read(resource_id) {
                Ok(seed) => seed,
                Err(error) => {
                    eprintln!("devmanager-host: terminal history for {resource_id} unreadable ({error}); deleting");
                    let _ = store.delete(resource_id);
                    None
                }
            },
            None => {
                let _ = store.delete(resource_id);
                None
            }
        };
```

Pass `seed.as_deref()` to `spawn_task_shell_session`; after a successful attach, `let _ = store.delete(resource_id);`. In the resource release completion path (where `ResourceReleased` is recorded for terminals), call `store.delete(resource_id)`. Handle `ActionRequest::TerminalHistoryClear` (new host command) with `store.delete_all()` and log the result.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib --locked host::connection::tests -- --test-threads=1`; `cargo check --locked --lib --bins --tests`
Expected: PASS; EXIT 0.

- [ ] **Step 5: Commit**

```bash
git add src/host/connection.rs src/services/process_manager.rs src/client/action.rs src/ui/native_shell.rs
git commit -m "feat(host): replay captured screen history on shell restore and delete it after use

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 6: End-to-end test, sabotage, docs

**Files:**
- Create: `tests/screen_history.rs`
- Modify: `docs/architecture.md`, `docs/superpowers/specs/2026-09-01-terminal-screen-history-design.md` (decisions log if anything deviated)

- [ ] **Step 1: End-to-end test**

Extend the phases of `tests/shell_restore.rs` (plan 2 Task 6): after the shell prints `Write-Host -ForegroundColor Red "old output"` (pwsh) or `echo old output` (cmd), issue a confirmed full quit through the client (`inspect_host_quit` + `confirm_host_quit` after closing agents), assert `<profile_root>/terminals/<resource_id>.ansi` exists and contains `old output`, start host B, attach, wait for the shell to be Running, and assert the grid `text_lines` contain `old output` above a line containing `restored`, and that the file is gone.

- [ ] **Step 2: Run it**

Run: `cargo test --locked --test screen_history -- --nocapture`
Expected: PASS (same caveat as plan 2 about the pre-existing `host_lifecycle` `Unavailable` failure: stop and report if it reproduces).

- [ ] **Step 3: Sabotage checks**

Remove the `alternate_screen` guard in `export_screen_ansi` → `alternate_screen_and_blank_export_nothing` must fail. Remove the `replaying` early-return in `SessionEventProxy`'s clipboard handler → `seeded_history_precedes_the_first_live_line_and_emits_no_side_effects` must fail. Revert both.

- [ ] **Step 4: Docs**

Append to `docs/architecture.md` "Host lifetime and terminal survivability":

```markdown
Plain shell screens are captured as styled ANSI at confirmed full quit and update handoff (2 s budget, 1 MiB per terminal, alternate-screen and blank screens skipped) into `<profile_root>/terminals/<resource_id>.ansi`, and replayed into the restored shell's grid before its PTY spawns, followed by a `── restored <time> ──` marker. Replayed bytes never emit clipboard, bell, title or cwd effects. The setting "Keep terminal screens across restarts" (default on) disables capture and deletes the directory when turned off.
```

Replace the sentence "shell screen contents are not persisted across a host restart" in that section with a pointer to this paragraph.

- [ ] **Step 5: Final gates and commit**

```powershell
cargo check --locked --lib --bins --tests
cargo test --lib --locked -- --test-threads=1 terminal:: host:: config::model domain:: kernel::
cargo test --locked --test terminal_service --test task_shell_terminals --test shell_restore --test screen_history
```

```bash
git add tests/screen_history.rs docs/architecture.md docs/superpowers/specs/2026-09-01-terminal-screen-history-design.md
git commit -m "test(host): screen history survives a graceful host restart; document capture and replay

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

## Self-review notes

- Spec §3.1 when → Task 4 (shutdown and updater hooks, budget, per-terminal share). §3.2 what → Task 1 (serializer, alt-screen/blank, cap) and Task 2 (`export_screen_history`). §3.3 where → Task 3 store and fact. §3.4 setting → Task 3. §4 replay → Task 2 (seed before spawn, marker, drain) and Task 5 (read, delete, setting off, oversized). §5 errors → Tasks 4 and 5. §6 tests → per task plus Task 6 sabotage and end-to-end.
- Names used across tasks: `export_screen_ansi`, `cap_history_from_top`, `MAX_HISTORY_BYTES`, `TerminalHistoryStore`, `CaptureReport`, `capture_terminal_histories`, `RecordTerminalHistoryCaptured`, `TerminalHistoryClear`, `history_marker_line`, `replaying`.
- `ensure_running(resource_id, seed)` keeps plan 2's signature; `None` now means "consult the store", an explicit `Some` bypasses it (used by tests).
