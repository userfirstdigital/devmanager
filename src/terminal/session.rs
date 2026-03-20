use crate::models::DefaultTerminal;
use crate::services::{pid_file, platform_service};
use crate::state::{
    RuntimeState, SessionDimensions, SessionExitState, SessionKind, SessionRuntimeState,
    SessionStatus,
};
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{point_to_viewport, Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape, NamedColor, Processor, Rgb, StdSyncHandler,
};
use arboard::Clipboard;
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

const MAX_TERMINAL_CLIPBOARD_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminalBackend {
    #[default]
    PortablePtyFeedingAlacritty,
}

impl TerminalBackend {
    pub fn label(self) -> &'static str {
        match self {
            Self::PortablePtyFeedingAlacritty => "portable_pty -> alacritty_terminal",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCursorSnapshot {
    pub row: usize,
    pub column: usize,
    pub shape: CursorShape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCellSnapshot {
    pub character: char,
    pub zero_width: Vec<char>,
    pub foreground: u32,
    pub background: u32,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub undercurl: bool,
    pub strike: bool,
    pub hidden: bool,
    pub has_hyperlink: bool,
    pub default_background: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalIndexedCellSnapshot {
    pub row: usize,
    pub column: usize,
    pub cell: TerminalCellSnapshot,
}

impl TerminalCellSnapshot {
    fn blank(foreground: u32, background: u32) -> Self {
        Self {
            character: ' ',
            zero_width: Vec::new(),
            foreground,
            background,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            undercurl: false,
            strike: false,
            hidden: false,
            has_hyperlink: false,
            default_background: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalModeSnapshot {
    pub alternate_screen: bool,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    pub focus_in_out: bool,
    pub mouse_report_click: bool,
    pub mouse_drag: bool,
    pub mouse_motion: bool,
    pub sgr_mouse: bool,
    pub utf8_mouse: bool,
    pub alternate_scroll: bool,
}

impl TerminalModeSnapshot {
    pub fn mouse_reporting(self) -> bool {
        self.mouse_report_click || self.mouse_drag || self.mouse_motion
    }
}

#[derive(Debug, Clone, Default)]
pub struct TerminalScreenSnapshot {
    pub cells: Vec<TerminalIndexedCellSnapshot>,
    pub lines: Vec<Vec<TerminalCellSnapshot>>,
    pub cursor: Option<TerminalCursorSnapshot>,
    pub display_offset: usize,
    pub rows: usize,
    pub cols: usize,
    pub mode: TerminalModeSnapshot,
}

#[derive(Debug, Clone)]
pub struct TerminalSessionView {
    pub runtime: SessionRuntimeState,
    pub screen: TerminalScreenSnapshot,
}

#[derive(Debug, Clone, Copy)]
struct TerminalSize {
    cols: usize,
    rows: usize,
}

impl TerminalSize {
    fn new(cols: usize, rows: usize) -> Self {
        Self { cols, rows }
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Clone)]
struct SessionEventProxy {
    session_id: String,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    runtime_state: Arc<RwLock<RuntimeState>>,
    dimensions: Arc<Mutex<SessionDimensions>>,
    debug_enabled: bool,
}

impl SessionEventProxy {
    fn write_to_pty(&self, text: &str) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(text.as_bytes());
            let _ = writer.flush();
        }
    }

    fn with_runtime(&self, f: impl FnOnce(&mut SessionRuntimeState)) {
        if let Ok(mut runtime) = self.runtime_state.write() {
            if let Some(session) = runtime.sessions.get_mut(&self.session_id) {
                f(session);
            }
        }
    }

    fn current_window_size(&self) -> WindowSize {
        let dimensions = self
            .dimensions
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        WindowSize {
            num_lines: dimensions.rows,
            num_cols: dimensions.cols,
            cell_width: dimensions.cell_width,
            cell_height: dimensions.cell_height,
        }
    }

    fn debug_log(&self, message: impl AsRef<str>) {
        if self.debug_enabled {
            eprintln!("[terminal:{}] {}", self.session_id, message.as_ref());
        }
    }
}

impl EventListener for SessionEventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => {
                self.debug_log(format!("title -> {title}"));
                self.with_runtime(|session| session.note_title(Some(title)));
            }
            Event::ResetTitle => {
                self.with_runtime(|session| session.note_title(None));
            }
            Event::Bell => {
                self.with_runtime(SessionRuntimeState::note_bell);
            }
            Event::PtyWrite(text) => {
                self.write_to_pty(&text);
            }
            Event::TextAreaSizeRequest(formatter) => {
                let response = formatter(self.current_window_size());
                self.write_to_pty(&response);
            }
            Event::CursorBlinkingChange | Event::MouseCursorDirty | Event::Wakeup => {
                self.with_runtime(SessionRuntimeState::mark_dirty);
            }
            Event::Exit => {
                self.debug_log("terminal requested exit");
                self.with_runtime(|session| {
                    session.note_exit(
                        SessionExitState {
                            code: None,
                            signal: None,
                            closed_by_user: false,
                            summary: "Terminal requested exit".to_string(),
                        },
                        SessionStatus::Exited,
                    );
                });
            }
            Event::ChildExit(code) => {
                self.with_runtime(|session| {
                    session.note_exit(
                        SessionExitState {
                            code: Some(code as u32),
                            signal: None,
                            closed_by_user: false,
                            summary: format!("Shell exited with code {code}"),
                        },
                        SessionStatus::Exited,
                    );
                });
            }
            Event::ColorRequest(index, formatter) => {
                let color = color_for_index(index);
                let response = formatter(color);
                self.write_to_pty(&response);
            }
            Event::ClipboardStore(_, data) => {
                let clipped = truncate_utf8_boundary(&data, MAX_TERMINAL_CLIPBOARD_BYTES).to_string();
                if let Err(error) = write_system_clipboard_text(&clipped) {
                    self.debug_log(format!("clipboard store failed: {error}"));
                }
            }
            Event::ClipboardLoad(_, formatter) => {
                let text = read_system_clipboard_text().unwrap_or_default();
                let response = formatter(&text);
                self.write_to_pty(&response);
            }
        }
    }
}

pub struct TerminalSession {
    session_id: String,
    term: Arc<Mutex<Term<SessionEventProxy>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    runtime_state: Arc<RwLock<RuntimeState>>,
    dimensions: Arc<Mutex<SessionDimensions>>,
    event_proxy: SessionEventProxy,
    backend: TerminalBackend,
    scrolling_history: Arc<RwLock<usize>>,
}

impl TerminalSession {
    pub fn spawn(
        session_id: impl Into<String>,
        cwd: PathBuf,
        dimensions: SessionDimensions,
        preferred_terminal: Option<DefaultTerminal>,
        scrolling_history: usize,
        runtime_state: Arc<RwLock<RuntimeState>>,
        debug_enabled: bool,
    ) -> Result<Self, String> {
        let session_id = session_id.into();
        let backend = TerminalBackend::PortablePtyFeedingAlacritty;

        let candidates = shell_candidates(preferred_terminal.as_ref());
        let mut last_error = None;

        for candidate in candidates {
            match spawn_with_command(
                &session_id,
                cwd.clone(),
                dimensions,
                candidate.program.to_string(),
                candidate.args.clone(),
                HashMap::new(),
                scrolling_history,
                None,
                runtime_state.clone(),
                debug_enabled,
                backend,
                true,
            ) {
                Ok(session) => return Ok(session),
                Err(error) => last_error = Some(format!("{}: {}", candidate.program, error)),
            }
        }

        Err(last_error.unwrap_or_else(|| "No shell candidate could be spawned".to_string()))
    }

    pub fn spawn_command(
        session_id: impl Into<String>,
        cwd: PathBuf,
        dimensions: SessionDimensions,
        program: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        scrolling_history: usize,
        log_file_path: Option<PathBuf>,
        runtime_state: Arc<RwLock<RuntimeState>>,
        debug_enabled: bool,
    ) -> Result<Self, String> {
        let session_id = session_id.into();
        spawn_with_command(
            &session_id,
            cwd,
            dimensions,
            program,
            args,
            env,
            scrolling_history,
            log_file_path,
            runtime_state,
            debug_enabled,
            TerminalBackend::PortablePtyFeedingAlacritty,
            true,
        )
    }

    pub fn backend(&self) -> TerminalBackend {
        self.backend
    }

    pub fn id(&self) -> &str {
        &self.session_id
    }

    pub fn write_bytes(&self, bytes: &[u8]) -> Result<(), String> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| "PTY writer poisoned".to_string())?;
        writer
            .write_all(bytes)
            .map_err(|error| format!("Failed to write to PTY: {error}"))?;
        writer
            .flush()
            .map_err(|error| format!("Failed to flush PTY input: {error}"))?;
        Ok(())
    }

    pub fn write_text(&self, text: &str) -> Result<(), String> {
        self.write_bytes(text.as_bytes())
    }

    pub fn paste_text(&self, text: &str) -> Result<(), String> {
        let bracketed_paste = {
            let term = self
                .term
                .lock()
                .map_err(|_| "Terminal state poisoned".to_string())?;
            term.mode().contains(TermMode::BRACKETED_PASTE)
        };

        if bracketed_paste {
            self.write_text(&format!(
                "\u{1b}[200~{}\u{1b}[201~",
                sanitize_bracketed_paste_text(text)
            ))
        } else {
            self.write_text(&normalize_plain_paste_text(text))
        }
    }

    pub fn resize(&self, dimensions: SessionDimensions) -> Result<(), String> {
        {
            let master = self
                .master
                .lock()
                .map_err(|_| "PTY master poisoned".to_string())?;
            master
                .resize(pty_size(dimensions))
                .map_err(|error| format!("Failed to resize PTY: {error}"))?;
        }

        {
            let mut current = self
                .dimensions
                .lock()
                .map_err(|_| "Size lock poisoned".to_string())?;
            *current = dimensions;
        }

        {
            let mut term = self
                .term
                .lock()
                .map_err(|_| "Terminal state poisoned".to_string())?;
            term.resize(TerminalSize::new(
                dimensions.cols as usize,
                dimensions.rows as usize,
            ));
        }

        if let Ok(mut runtime) = self.runtime_state.write() {
            if let Some(session) = runtime.sessions.get_mut(&self.session_id) {
                session.note_resize(dimensions);
            }
        }

        Ok(())
    }

    pub fn scroll(&self, delta_lines: i32) -> Result<(), String> {
        let display_offset = {
            let mut term = self
                .term
                .lock()
                .map_err(|_| "Terminal state poisoned".to_string())?;
            term.scroll_display(Scroll::Delta(delta_lines));
            term.grid().display_offset()
        };

        if let Ok(mut runtime) = self.runtime_state.write() {
            if let Some(session) = runtime.sessions.get_mut(&self.session_id) {
                session.note_scroll(display_offset);
            }
        }

        Ok(())
    }

    pub fn close(&self, closed_by_user: bool) -> Result<(), String> {
        if let Ok(mut runtime) = self.runtime_state.write() {
            if let Some(session) = runtime.sessions.get_mut(&self.session_id) {
                session.status = SessionStatus::Stopping;
                session.exit = Some(SessionExitState {
                    code: None,
                    signal: None,
                    closed_by_user,
                    summary: if closed_by_user {
                        "Session close requested by user".to_string()
                    } else {
                        "Session close requested".to_string()
                    },
                });
                session.mark_dirty();
            }
        }

        let mut killer = self
            .killer
            .lock()
            .map_err(|_| "Session killer poisoned".to_string())?;
        killer
            .kill()
            .map_err(|error| format!("Failed to terminate shell session: {error}"))
    }

    pub fn restart_command(
        &self,
        cwd: PathBuf,
        dimensions: SessionDimensions,
        program: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        log_file_path: Option<PathBuf>,
        track_pid: bool,
    ) -> Result<(), String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(pty_size(dimensions))
            .map_err(|error| error.to_string())?;
        let mut command = CommandBuilder::new(program.clone());
        if let Some(valid_cwd) = existing_directory(&cwd) {
            command.cwd(valid_cwd);
        }
        if !args.is_empty() {
            command.args(args.clone());
        }
        apply_terminal_env_defaults(&mut command, env);

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| format!("Failed to spawn command: {error}"))?;

        let pid = child.process_id();
        let mut cleanup_killer = child.clone_killer();

        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(error) => {
                cleanup_failed_spawn(&mut cleanup_killer);
                return Err(format!("Failed to acquire PTY writer: {error}"));
            }
        };
        let reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(error) => {
                cleanup_failed_spawn(&mut cleanup_killer);
                return Err(format!("Failed to clone PTY reader: {error}"));
            }
        };
        let log_writer = open_log_writer(log_file_path);

        {
            let mut writer_slot = match self.writer.lock() {
                Ok(writer_slot) => writer_slot,
                Err(_) => {
                    cleanup_failed_spawn(&mut cleanup_killer);
                    return Err("PTY writer poisoned".to_string());
                }
            };
            *writer_slot = writer;
        }
        {
            let mut master_slot = match self.master.lock() {
                Ok(master_slot) => master_slot,
                Err(_) => {
                    cleanup_failed_spawn(&mut cleanup_killer);
                    return Err("PTY master poisoned".to_string());
                }
            };
            *master_slot = pair.master;
        }
        {
            let mut killer_slot = match self.killer.lock() {
                Ok(killer_slot) => killer_slot,
                Err(_) => {
                    cleanup_failed_spawn(&mut cleanup_killer);
                    return Err("Session killer poisoned".to_string());
                }
            };
            *killer_slot = child.clone_killer();
        }
        {
            let mut current_dimensions = match self.dimensions.lock() {
                Ok(current_dimensions) => current_dimensions,
                Err(_) => {
                    cleanup_failed_spawn(&mut cleanup_killer);
                    return Err("Size lock poisoned".to_string());
                }
            };
            *current_dimensions = dimensions;
        }
        {
            let mut term = match self.term.lock() {
                Ok(term) => term,
                Err(_) => {
                    cleanup_failed_spawn(&mut cleanup_killer);
                    return Err("Terminal state poisoned".to_string());
                }
            };
            term.resize(TerminalSize::new(
                dimensions.cols as usize,
                dimensions.rows as usize,
            ));
        }

        initialize_runtime_entry(
            &self.runtime_state,
            &self.session_id,
            cwd.clone(),
            dimensions,
            program.clone(),
            self.backend,
            pid,
        );

        if track_pid {
            if let Err(error) =
                track_managed_process(&self.runtime_state, &self.session_id, pid, &program)
            {
                cleanup_failed_spawn(&mut cleanup_killer);
                return Err(error);
            }
        }

        spawn_reader_thread(
            self.session_id.clone(),
            reader,
            self.term.clone(),
            log_writer,
            self.runtime_state.clone(),
            self.event_proxy.debug_enabled,
        );
        spawn_wait_thread(
            self.session_id.clone(),
            child,
            pid,
            self.runtime_state.clone(),
            self.event_proxy.debug_enabled,
        );

        self.event_proxy.debug_log(format!("respawned {}", program));
        Ok(())
    }

    pub fn write_virtual_text(&self, text: &str) {
        let mut parser = Processor::<StdSyncHandler>::new();
        let mut term = match self.term.lock() {
            Ok(term) => term,
            Err(error) => error.into_inner(),
        };
        parser.advance(&mut *term, text.as_bytes());
    }

    pub fn set_scrollback_lines(&self, lines: usize) {
        let lines = lines.max(100);
        if let Ok(mut scrollback) = self.scrolling_history.write() {
            *scrollback = lines;
        }
        let mut term = match self.term.lock() {
            Ok(term) => term,
            Err(error) => error.into_inner(),
        };
        term.set_options(configured_term(lines));
        self.event_proxy.with_runtime(|session| {
            session.display_offset = session.display_offset.min(lines);
            session.mark_dirty();
        });
    }

    pub fn clear_virtual_output(&self) {
        let dimensions = self
            .dimensions
            .lock()
            .map(|dimensions| *dimensions)
            .unwrap_or_default();
        let mut term = match self.term.lock() {
            Ok(term) => term,
            Err(error) => error.into_inner(),
        };
        let scrolling_history = self
            .scrolling_history
            .read()
            .map(|lines| *lines)
            .unwrap_or(10_000);
        *term = Term::new(
            configured_term(scrolling_history),
            &TerminalSize::new(dimensions.cols as usize, dimensions.rows as usize),
            self.event_proxy.clone(),
        );
    }

    pub fn mode_snapshot(&self) -> TerminalModeSnapshot {
        let term = match self.term.lock() {
            Ok(term) => term,
            Err(error) => error.into_inner(),
        };
        mode_snapshot(*term.mode())
    }

    pub fn report_focus(&self, focused: bool) -> Result<(), String> {
        if !self.mode_snapshot().focus_in_out {
            return Ok(());
        }
        self.write_text(if focused { "\u{1b}[I" } else { "\u{1b}[O" })
    }

    pub fn snapshot(&self) -> TerminalScreenSnapshot {
        let term = match self.term.lock() {
            Ok(term) => term,
            Err(error) => error.into_inner(),
        };
        snapshot_term(&term)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.close(false);
    }
}

#[derive(Clone)]
struct ShellCandidate {
    program: String,
    args: Vec<String>,
}

fn shell_candidates(preferred_terminal: Option<&DefaultTerminal>) -> Vec<ShellCandidate> {
    let preferred_terminal = preferred_terminal.cloned().unwrap_or_default();
    if cfg!(target_os = "windows") {
        match preferred_terminal {
            DefaultTerminal::Cmd => vec![
                ShellCandidate {
                    program: "cmd.exe".to_string(),
                    args: Vec::new(),
                },
                ShellCandidate {
                    program: "pwsh".to_string(),
                    args: vec!["-NoLogo".to_string()],
                },
                ShellCandidate {
                    program: "powershell.exe".to_string(),
                    args: vec!["-NoLogo".to_string()],
                },
            ],
            DefaultTerminal::Powershell => vec![
                ShellCandidate {
                    program: "pwsh".to_string(),
                    args: vec!["-NoLogo".to_string()],
                },
                ShellCandidate {
                    program: "powershell.exe".to_string(),
                    args: vec!["-NoLogo".to_string()],
                },
                ShellCandidate {
                    program: "cmd.exe".to_string(),
                    args: Vec::new(),
                },
            ],
            DefaultTerminal::Bash => vec![
                ShellCandidate {
                    program: preferred_windows_bash_program(),
                    args: vec!["--login".to_string()],
                },
                ShellCandidate {
                    program: "bash".to_string(),
                    args: vec!["--login".to_string()],
                },
                ShellCandidate {
                    program: "pwsh".to_string(),
                    args: vec!["-NoLogo".to_string()],
                },
                ShellCandidate {
                    program: "powershell.exe".to_string(),
                    args: vec!["-NoLogo".to_string()],
                },
                ShellCandidate {
                    program: "cmd.exe".to_string(),
                    args: Vec::new(),
                },
            ],
        }
    } else {
        match preferred_terminal {
            DefaultTerminal::Powershell => vec![
                ShellCandidate {
                    program: "pwsh".to_string(),
                    args: Vec::new(),
                },
                ShellCandidate {
                    program: "bash".to_string(),
                    args: Vec::new(),
                },
                ShellCandidate {
                    program: "zsh".to_string(),
                    args: Vec::new(),
                },
                ShellCandidate {
                    program: "sh".to_string(),
                    args: Vec::new(),
                },
            ],
            _ => vec![
                ShellCandidate {
                    program: "bash".to_string(),
                    args: Vec::new(),
                },
                ShellCandidate {
                    program: "zsh".to_string(),
                    args: Vec::new(),
                },
                ShellCandidate {
                    program: "sh".to_string(),
                    args: Vec::new(),
                },
                ShellCandidate {
                    program: "pwsh".to_string(),
                    args: Vec::new(),
                },
            ],
        }
    }
}

pub fn preferred_windows_bash_program() -> String {
    std::env::var("DEVMANAGER_GIT_BASH")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            [
                "C:/Program Files/Git/bin/bash.exe",
                "C:/Program Files (x86)/Git/bin/bash.exe",
            ]
            .iter()
            .find(|path| Path::new(path).exists())
            .map(|path| (*path).to_string())
        })
        .unwrap_or_else(|| "bash".to_string())
}

fn renderable_char(cell: &Cell) -> char {
    if cell.flags.contains(Flags::HIDDEN) {
        ' '
    } else {
        cell.c
    }
}

fn snapshot_term(term: &Term<SessionEventProxy>) -> TerminalScreenSnapshot {
    let content = term.renderable_content();
    let display_offset = content.display_offset;
    let rows = term.screen_lines();
    let cols = term.columns();
    let mode = mode_snapshot(content.mode);
    let cursor = if content.cursor.shape == CursorShape::Hidden {
        None
    } else {
        point_to_viewport(display_offset, content.cursor.point).map(|point| {
            TerminalCursorSnapshot {
                row: point.line,
                column: point.column.0,
                shape: content.cursor.shape,
            }
        })
    };

    let default_foreground =
        resolve_terminal_color(AnsiColor::Named(NamedColor::Foreground), content.colors);
    let default_background =
        resolve_terminal_color(AnsiColor::Named(NamedColor::Background), content.colors);
    let mut grid_lines =
        vec![vec![TerminalCellSnapshot::blank(default_foreground, default_background); cols]; rows];
    let mut indexed_cells = Vec::with_capacity(content.display_iter.size_hint().0);
    for indexed in content.display_iter {
        let Some(point) = point_to_viewport(display_offset, indexed.point) else {
            continue;
        };
        if point.line >= rows || point.column.0 >= cols {
            continue;
        }

        if indexed.cell.flags.contains(Flags::WIDE_CHAR_SPACER)
            || indexed.cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }

        let cell = renderable_cell_snapshot(indexed.cell, content.colors);
        grid_lines[point.line][point.column.0] = cell.clone();
        indexed_cells.push(TerminalIndexedCellSnapshot {
            row: point.line,
            column: point.column.0,
            cell,
        });
    }

    TerminalScreenSnapshot {
        cells: indexed_cells,
        lines: grid_lines,
        cursor,
        display_offset,
        rows,
        cols,
        mode,
    }
}

fn configured_term(scrolling_history: usize) -> TermConfig {
    TermConfig {
        scrolling_history,
        ..Default::default()
    }
}

fn apply_terminal_env_defaults(command: &mut CommandBuilder, env: HashMap<String, String>) {
    command.env_remove("NO_COLOR");
    command.env_remove("NODE_DISABLE_COLORS");
    for (key, value) in with_terminal_env_defaults(env) {
        command.env(key, value);
    }
}

fn with_terminal_env_defaults(mut env: HashMap<String, String>) -> HashMap<String, String> {
    env.entry("TERM".to_string())
        .or_insert_with(|| "xterm-256color".to_string());
    env.entry("COLORTERM".to_string())
        .or_insert_with(|| "truecolor".to_string());
    env.entry("TERM_PROGRAM".to_string())
        .or_insert_with(|| "DevManager".to_string());
    env.entry("TERM_PROGRAM_VERSION".to_string())
        .or_insert_with(|| env!("CARGO_PKG_VERSION").to_string());
    env.entry("CLICOLOR".to_string())
        .or_insert_with(|| "1".to_string());
    env.entry("CLICOLOR_FORCE".to_string())
        .or_insert_with(|| "1".to_string());
    env.entry("FORCE_COLOR".to_string())
        .or_insert_with(|| "1".to_string());
    env
}

fn sanitize_bracketed_paste_text(text: &str) -> String {
    text.chars()
        .filter(|ch| *ch != '\u{1b}' && *ch != '\u{9b}')
        .collect()
}

fn normalize_plain_paste_text(text: &str) -> String {
    text.replace("\r\n", "\r").replace('\n', "\r")
}

fn truncate_utf8_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }

    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &text[..end]
}

fn read_system_clipboard_text() -> Option<String> {
    let mut clipboard = Clipboard::new().ok()?;
    let text = clipboard.get_text().ok()?;
    Some(truncate_utf8_boundary(&text, MAX_TERMINAL_CLIPBOARD_BYTES).to_string())
}

fn write_system_clipboard_text(text: &str) -> Result<(), String> {
    let mut clipboard = Clipboard::new().map_err(|error| format!("Failed to open clipboard: {error}"))?;
    clipboard
        .set_text(truncate_utf8_boundary(text, MAX_TERMINAL_CLIPBOARD_BYTES).to_string())
        .map_err(|error| format!("Failed to write clipboard: {error}"))
}

fn pty_size(dimensions: SessionDimensions) -> PtySize {
    PtySize {
        rows: dimensions.rows,
        cols: dimensions.cols,
        pixel_width: dimensions.cell_width,
        pixel_height: dimensions.cell_height,
    }
}

fn existing_directory(path: &Path) -> Option<&Path> {
    path.is_dir().then_some(path)
}

fn session_kind_label(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Shell => "shell",
        SessionKind::Server => "server",
        SessionKind::Claude => "claude",
        SessionKind::Codex => "codex",
        SessionKind::Ssh => "ssh",
    }
}

fn capture_process_identity_with_retry(pid: u32) -> Option<platform_service::ProcessIdentity> {
    for _ in 0..20 {
        if let Some(identity) = platform_service::capture_process_identity(pid) {
            return Some(identity);
        }
        thread::sleep(Duration::from_millis(25));
    }
    None
}

fn track_managed_process(
    runtime_state: &Arc<RwLock<RuntimeState>>,
    session_id: &str,
    pid: Option<u32>,
    program: &str,
) -> Result<(), String> {
    let Some(pid) = pid else {
        return Ok(());
    };
    let identity = capture_process_identity_with_retry(pid)
        .ok_or_else(|| format!("Failed to capture process identity for `{session_id}`"))?;
    let session = runtime_state
        .read()
        .map_err(|_| "Runtime state poisoned".to_string())?
        .sessions
        .get(session_id)
        .cloned()
        .ok_or_else(|| format!("Missing runtime session `{session_id}` for process tracking"))?;
    pid_file::track_session_process(pid_file::ManagedProcessRecord {
        session_id: session_id.to_string(),
        pid,
        started_at_unix_secs: identity.started_at_unix_secs,
        process_name: identity.process_name,
        session_kind: session_kind_label(session.session_kind).to_string(),
        program: program.to_string(),
        project_id: session.project_id.clone(),
        command_id: session.command_id.clone(),
        tab_id: session.tab_id.clone(),
    })
}

fn cleanup_failed_spawn(cleanup_killer: &mut Box<dyn ChildKiller + Send + Sync>) {
    let _ = cleanup_killer.kill();
}

fn initialize_runtime_entry(
    runtime_state: &Arc<RwLock<RuntimeState>>,
    session_id: &str,
    cwd: PathBuf,
    dimensions: SessionDimensions,
    shell_program: String,
    backend: TerminalBackend,
    pid: Option<u32>,
) {
    if let Ok(mut runtime) = runtime_state.write() {
        let entry = runtime
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| {
                SessionRuntimeState::new(session_id.to_string(), cwd.clone(), dimensions, backend)
            });
        entry.cwd = cwd;
        entry.dimensions = dimensions;
        entry.note_start(pid);
        entry.shell_program = shell_program;
        entry.backend = backend;
        entry.exit = None;
        entry.mark_dirty();
    }
}

fn spawn_reader_thread(
    session_id: String,
    mut reader: Box<dyn Read + Send>,
    term: Arc<Mutex<Term<SessionEventProxy>>>,
    mut log_writer: Option<LogWriter>,
    runtime_state: Arc<RwLock<RuntimeState>>,
    debug_enabled: bool,
) {
    thread::spawn(move || {
        let mut parser = Processor::<StdSyncHandler>::new();
        let mut buffer = [0_u8; 4096];

        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    if debug_enabled {
                        eprintln!("[terminal:{session_id}] PTY reader reached EOF");
                    }
                    break;
                }
                Ok(bytes_read) => {
                    {
                        let mut term = match term.lock() {
                            Ok(term) => term,
                            Err(error) => error.into_inner(),
                        };
                        parser.advance(&mut *term, &buffer[..bytes_read]);
                    }

                    if let Some(writer) = log_writer.as_mut() {
                        writer.write_chunk(&buffer[..bytes_read]);
                    }

                    if let Ok(mut runtime) = runtime_state.write() {
                        if let Some(session) = runtime.sessions.get_mut(&session_id) {
                            session.record_pty_bytes(bytes_read);
                            session.note_output_activity();
                        }
                    }
                }
                Err(error) => {
                    if debug_enabled {
                        eprintln!("[terminal:{session_id}] PTY read error: {error}");
                    }
                    if let Ok(mut runtime) = runtime_state.write() {
                        if let Some(session) = runtime.sessions.get_mut(&session_id) {
                            session.note_exit(
                                SessionExitState {
                                    code: None,
                                    signal: None,
                                    closed_by_user: false,
                                    summary: format!("PTY read failed: {error}"),
                                },
                                SessionStatus::Failed,
                            );
                        }
                    }
                    break;
                }
            }
        }

        if let Some(writer) = log_writer.as_mut() {
            writer.flush_remaining();
        }
    });
}

fn spawn_wait_thread(
    session_id: String,
    mut child: Box<dyn Child + Send + Sync>,
    pid: Option<u32>,
    runtime_state: Arc<RwLock<RuntimeState>>,
    debug_enabled: bool,
) {
    thread::spawn(move || match child.wait() {
        Ok(status) => {
            if debug_enabled {
                eprintln!("[terminal:{session_id}] child exit -> {status}");
            }
            if let Ok(mut runtime) = runtime_state.write() {
                if let Some(session) = runtime.sessions.get_mut(&session_id) {
                    let closed_by_user = session
                        .exit
                        .as_ref()
                        .map(|exit| exit.closed_by_user)
                        .unwrap_or(false);
                    session.note_exit(
                        SessionExitState {
                            code: Some(status.exit_code()),
                            signal: status.signal().map(str::to_string),
                            closed_by_user,
                            summary: if let Some(signal) = status.signal() {
                                format!("Shell terminated by {signal}")
                            } else {
                                format!("Shell exited with code {}", status.exit_code())
                            },
                        },
                        SessionStatus::Exited,
                    );
                }
            }
            if let Some(pid) = pid {
                let _ = pid_file::untrack_session_process(&session_id, pid);
            }
        }
        Err(error) => {
            if debug_enabled {
                eprintln!("[terminal:{session_id}] wait error: {error}");
            }
            if let Ok(mut runtime) = runtime_state.write() {
                if let Some(session) = runtime.sessions.get_mut(&session_id) {
                    session.note_exit(
                        SessionExitState {
                            code: None,
                            signal: None,
                            closed_by_user: false,
                            summary: format!("Failed while waiting for shell exit: {error}"),
                        },
                        SessionStatus::Failed,
                    );
                }
            }
            if let Some(pid) = pid {
                let _ = pid_file::untrack_session_process(&session_id, pid);
            }
        }
    });
}

fn spawn_with_command(
    session_id: &str,
    cwd: PathBuf,
    dimensions: SessionDimensions,
    program: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    scrolling_history: usize,
    log_file_path: Option<PathBuf>,
    runtime_state: Arc<RwLock<RuntimeState>>,
    debug_enabled: bool,
    backend: TerminalBackend,
    track_pid: bool,
) -> Result<TerminalSession, String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(pty_size(dimensions))
        .map_err(|error| error.to_string())?;
    let scrolling_history = scrolling_history.max(100);

    let mut command = CommandBuilder::new(program.clone());
    if let Some(valid_cwd) = existing_directory(&cwd) {
        command.cwd(valid_cwd);
    }
    if !args.is_empty() {
        command.args(args.clone());
    }
    apply_terminal_env_defaults(&mut command, env);

    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("Failed to spawn command: {error}"))?;

    let pid = child.process_id();
    let mut cleanup_killer = child.clone_killer();
    let writer = match pair.master.take_writer() {
        Ok(writer) => writer,
        Err(error) => {
            cleanup_failed_spawn(&mut cleanup_killer);
            return Err(format!("Failed to acquire PTY writer: {error}"));
        }
    };
    let reader = match pair.master.try_clone_reader() {
        Ok(reader) => reader,
        Err(error) => {
            cleanup_failed_spawn(&mut cleanup_killer);
            return Err(format!("Failed to clone PTY reader: {error}"));
        }
    };
    let log_writer = open_log_writer(log_file_path);

    let writer = Arc::new(Mutex::new(writer));
    let master = Arc::new(Mutex::new(pair.master));
    let killer = Arc::new(Mutex::new(child.clone_killer()));
    let dimensions_state = Arc::new(Mutex::new(dimensions));
    let event_proxy = SessionEventProxy {
        session_id: session_id.to_string(),
        writer: writer.clone(),
        runtime_state: runtime_state.clone(),
        dimensions: dimensions_state.clone(),
        debug_enabled,
    };

    let term = Arc::new(Mutex::new(Term::new(
        configured_term(scrolling_history),
        &TerminalSize::new(dimensions.cols as usize, dimensions.rows as usize),
        event_proxy.clone(),
    )));
    let scrolling_history = Arc::new(RwLock::new(scrolling_history));

    initialize_runtime_entry(
        &runtime_state,
        session_id,
        cwd.clone(),
        dimensions,
        program.clone(),
        backend,
        pid,
    );

    if track_pid {
        if let Err(error) = track_managed_process(&runtime_state, session_id, pid, &program) {
            cleanup_failed_spawn(&mut cleanup_killer);
            return Err(error);
        }
    }

    spawn_reader_thread(
        session_id.to_string(),
        reader,
        term.clone(),
        log_writer,
        runtime_state.clone(),
        debug_enabled,
    );

    spawn_wait_thread(
        session_id.to_string(),
        child,
        pid,
        runtime_state.clone(),
        debug_enabled,
    );

    event_proxy.debug_log(format!("spawned {}", program));

    Ok(TerminalSession {
        session_id: session_id.to_string(),
        term,
        writer,
        master,
        killer,
        runtime_state,
        dimensions: dimensions_state,
        event_proxy,
        backend,
        scrolling_history,
    })
}

fn open_log_writer(log_file_path: Option<PathBuf>) -> Option<LogWriter> {
    let path = log_file_path?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::File::create(&path) {
        Ok(file) => Some(LogWriter::new(file)),
        Err(_) => None,
    }
}

struct LogWriter {
    writer: std::io::BufWriter<std::fs::File>,
    line_buf: Vec<u8>,
    ansi_re: regex::Regex,
}

impl LogWriter {
    fn new(file: std::fs::File) -> Self {
        Self {
            writer: std::io::BufWriter::new(file),
            line_buf: Vec::new(),
            ansi_re: regex::Regex::new(
                r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*\x07|\x1b[()][0-9A-Z]|\x0f",
            )
            .unwrap(),
        }
    }

    fn write_chunk(&mut self, chunk: &[u8]) {
        let text = String::from_utf8_lossy(chunk);
        let clean = self.ansi_re.replace_all(&text, "");
        for ch in clean.bytes() {
            match ch {
                b'\n' => {
                    let ts = time::OffsetDateTime::now_local()
                        .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
                    let line = String::from_utf8_lossy(&self.line_buf);
                    let _ = write!(
                        self.writer,
                        "[{:04}-{:02}-{:02} {:02}:{:02}:{:02}] {}\n",
                        ts.year(),
                        ts.month() as u8,
                        ts.day(),
                        ts.hour(),
                        ts.minute(),
                        ts.second(),
                        line.trim_end()
                    );
                    self.line_buf.clear();
                }
                b'\r' => {}
                _ => self.line_buf.push(ch),
            }
        }
    }

    fn flush_remaining(&mut self) {
        if !self.line_buf.is_empty() {
            let ts = time::OffsetDateTime::now_local()
                .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
            let line = String::from_utf8_lossy(&self.line_buf);
            let _ = write!(
                self.writer,
                "[{:04}-{:02}-{:02} {:02}:{:02}:{:02}] {}\n",
                ts.year(),
                ts.month() as u8,
                ts.day(),
                ts.hour(),
                ts.minute(),
                ts.second(),
                line.trim_end()
            );
            self.line_buf.clear();
        }
        let _ = self.writer.flush();
    }
}

fn renderable_cell_snapshot(cell: &Cell, colors: &Colors) -> TerminalCellSnapshot {
    let mut foreground = resolve_terminal_color(cell.fg, colors);
    let mut background = resolve_terminal_color(cell.bg, colors);
    let default_background = if cell.flags.contains(Flags::INVERSE) {
        matches!(cell.fg, AnsiColor::Named(NamedColor::Background))
    } else {
        matches!(cell.bg, AnsiColor::Named(NamedColor::Background))
    };

    if cell.flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut foreground, &mut background);
    }

    let bold = cell.flags.intersects(Flags::BOLD | Flags::DIM_BOLD);
    let dim = cell.flags.intersects(Flags::DIM | Flags::DIM_BOLD);

    TerminalCellSnapshot {
        character: renderable_char(cell),
        zero_width: cell.zerowidth().unwrap_or(&[]).to_vec(),
        foreground,
        background,
        bold,
        dim,
        italic: cell.flags.intersects(Flags::ITALIC | Flags::BOLD_ITALIC),
        underline: cell.flags.intersects(Flags::ALL_UNDERLINES) || cell.hyperlink().is_some(),
        undercurl: cell.flags.contains(Flags::UNDERCURL),
        strike: cell.flags.contains(Flags::STRIKEOUT),
        hidden: cell.flags.contains(Flags::HIDDEN),
        has_hyperlink: cell.hyperlink().is_some(),
        default_background,
    }
}

fn resolve_terminal_color(color: AnsiColor, colors: &Colors) -> u32 {
    match color {
        AnsiColor::Spec(rgb) => rgb_to_u32(rgb),
        AnsiColor::Indexed(index) => colors[index as usize]
            .map(rgb_to_u32)
            .unwrap_or_else(|| indexed_color_fallback(index)),
        AnsiColor::Named(name) => colors[name]
            .map(rgb_to_u32)
            .unwrap_or_else(|| named_color_fallback(name)),
    }
}

fn rgb_to_u32(rgb: Rgb) -> u32 {
    ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32
}

fn dim_color(color: u32) -> u32 {
    let red = (((color >> 16) & 0xff) as f32 * 0.7) as u32;
    let green = (((color >> 8) & 0xff) as f32 * 0.7) as u32;
    let blue = ((color & 0xff) as f32 * 0.7) as u32;
    (red << 16) | (green << 8) | blue
}

fn indexed_color_fallback(index: u8) -> u32 {
    match index {
        0 => 0x18181b,
        1 => 0xef4444,
        2 => 0x22c55e,
        3 => 0xeab308,
        4 => 0x3b82f6,
        5 => 0xa855f7,
        6 => 0x06b6d4,
        7 => 0xe4e4e7,
        8 => 0x52525b,
        9 => 0xf87171,
        10 => 0x4ade80,
        11 => 0xfacc15,
        12 => 0x60a5fa,
        13 => 0xc084fc,
        14 => 0x22d3ee,
        15 => 0xfafafa,
        16..=231 => {
            let cube = index - 16;
            let red = cube / 36;
            let green = (cube % 36) / 6;
            let blue = cube % 6;
            let channel = |value: u8| {
                if value == 0 {
                    0
                } else {
                    55 + value as u32 * 40
                }
            };
            (channel(red) << 16) | (channel(green) << 8) | channel(blue)
        }
        232..=255 => {
            let shade = 8 + (index as u32 - 232) * 10;
            (shade << 16) | (shade << 8) | shade
        }
    }
}

fn named_color_fallback(name: NamedColor) -> u32 {
    match name {
        NamedColor::Black => 0x18181b,
        NamedColor::Red => 0xef4444,
        NamedColor::Green => 0x22c55e,
        NamedColor::Yellow => 0xeab308,
        NamedColor::Blue => 0x3b82f6,
        NamedColor::Magenta => 0xa855f7,
        NamedColor::Cyan => 0x06b6d4,
        NamedColor::White => 0xe4e4e7,
        NamedColor::BrightBlack => 0x52525b,
        NamedColor::BrightRed => 0xf87171,
        NamedColor::BrightGreen => 0x4ade80,
        NamedColor::BrightYellow => 0xfacc15,
        NamedColor::BrightBlue => 0x60a5fa,
        NamedColor::BrightMagenta => 0xc084fc,
        NamedColor::BrightCyan => 0x22d3ee,
        NamedColor::BrightWhite => 0xfafafa,
        NamedColor::Foreground | NamedColor::BrightForeground => 0xe4e4e7,
        NamedColor::Background => crate::theme::TERMINAL_BG,
        NamedColor::Cursor => 0xe4e4e7,
        NamedColor::DimBlack => dim_color(0x18181b),
        NamedColor::DimRed => dim_color(0xef4444),
        NamedColor::DimGreen => dim_color(0x22c55e),
        NamedColor::DimYellow => dim_color(0xeab308),
        NamedColor::DimBlue => dim_color(0x3b82f6),
        NamedColor::DimMagenta => dim_color(0xa855f7),
        NamedColor::DimCyan => dim_color(0x06b6d4),
        NamedColor::DimWhite | NamedColor::DimForeground => dim_color(0xe4e4e7),
    }
}

fn u32_to_rgb(color: u32) -> Rgb {
    Rgb {
        r: ((color >> 16) & 0xff) as u8,
        g: ((color >> 8) & 0xff) as u8,
        b: (color & 0xff) as u8,
    }
}

fn color_for_index(index: usize) -> Rgb {
    let color = if index < 256 {
        indexed_color_fallback(index as u8)
    } else {
        match index {
            256 => named_color_fallback(NamedColor::Foreground),
            257 => named_color_fallback(NamedColor::Background),
            258 => named_color_fallback(NamedColor::Cursor),
            _ => 0xe4e4e7,
        }
    };
    u32_to_rgb(color)
}

fn mode_snapshot(mode: TermMode) -> TerminalModeSnapshot {
    TerminalModeSnapshot {
        alternate_screen: contains_mode(mode, "ALT_SCREEN"),
        app_cursor: contains_mode(mode, "APP_CURSOR"),
        bracketed_paste: contains_mode(mode, "BRACKETED_PASTE"),
        focus_in_out: contains_mode(mode, "FOCUS_IN_OUT"),
        mouse_report_click: contains_mode(mode, "MOUSE_REPORT_CLICK"),
        mouse_drag: contains_mode(mode, "MOUSE_DRAG"),
        mouse_motion: contains_mode(mode, "MOUSE_MOTION"),
        sgr_mouse: contains_mode(mode, "SGR_MOUSE"),
        utf8_mouse: contains_mode(mode, "UTF8_MOUSE"),
        alternate_scroll: contains_mode(mode, "ALTERNATE_SCROLL"),
    }
}

fn contains_mode(mode: TermMode, name: &str) -> bool {
    TermMode::from_name(name)
        .map(|flag| mode.contains(flag))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn test_event_proxy(dimensions: SessionDimensions) -> SessionEventProxy {
        SessionEventProxy {
            session_id: "test".to_string(),
            writer: Arc::new(Mutex::new(Box::new(io::sink()) as Box<dyn Write + Send>)),
            runtime_state: Arc::new(RwLock::new(RuntimeState::default())),
            dimensions: Arc::new(Mutex::new(dimensions)),
            debug_enabled: false,
        }
    }

    #[test]
    fn snapshot_preserves_ansi_color_cells() {
        let dimensions = SessionDimensions {
            cols: 8,
            rows: 2,
            cell_width: 8,
            cell_height: 16,
        };
        let proxy = test_event_proxy(dimensions);
        let mut term = Term::new(configured_term(1000), &TerminalSize::new(8, 2), proxy);
        let mut parser = Processor::<StdSyncHandler>::new();

        parser.advance(&mut term, b"\x1b[31mR\x1b[32mG\x1b[0mW");

        let snapshot = snapshot_term(&term);
        let red = &snapshot.lines[0][0];
        let green = &snapshot.lines[0][1];
        let default = &snapshot.lines[0][2];

        assert_eq!(red.character, 'R');
        assert_eq!(green.character, 'G');
        assert_eq!(default.character, 'W');
        assert_ne!(red.foreground, default.foreground);
        assert_ne!(green.foreground, default.foreground);
        assert_ne!(red.foreground, green.foreground);
    }

    #[test]
    fn terminal_env_defaults_force_color_output() {
        let env = with_terminal_env_defaults(HashMap::new());

        assert_eq!(env.get("TERM").map(String::as_str), Some("xterm-256color"));
        assert_eq!(env.get("COLORTERM").map(String::as_str), Some("truecolor"));
        assert_eq!(env.get("CLICOLOR").map(String::as_str), Some("1"));
        assert_eq!(env.get("CLICOLOR_FORCE").map(String::as_str), Some("1"));
        assert_eq!(env.get("FORCE_COLOR").map(String::as_str), Some("1"));
    }

    #[test]
    fn bracketed_paste_strips_escape_bytes() {
        let sanitized = sanitize_bracketed_paste_text("hello\u{1b}[31mworld\u{9b}200~");

        assert_eq!(sanitized, "hello[31mworld200~");
    }

    #[test]
    fn plain_paste_normalizes_newlines_to_carriage_returns() {
        let normalized = normalize_plain_paste_text("one\r\ntwo\nthree");

        assert_eq!(normalized, "one\rtwo\rthree");
    }

    #[test]
    fn truncate_utf8_boundary_does_not_split_multibyte_chars() {
        let text = "a😀b";

        assert_eq!(truncate_utf8_boundary(text, 2), "a");
        assert_eq!(truncate_utf8_boundary(text, 5), "a😀");
    }
}
