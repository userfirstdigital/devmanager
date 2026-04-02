use crate::models::DefaultTerminal;
use crate::services::{pid_file, platform_service};
use crate::state::{
    PromptMarkKind, RuntimeState, SessionDimensions, SessionExitState, SessionKind,
    SessionRuntimeState, SessionStatus, ShellIntegrationKind,
};
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Line;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{point_to_viewport, Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape, NamedColor, Processor, Rgb, StdSyncHandler,
};
use arboard::Clipboard;
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

const MAX_TERMINAL_CLIPBOARD_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalCursorSnapshot {
    pub row: usize,
    pub column: usize,
    #[serde(with = "cursor_shape_serde")]
    pub shape: CursorShape,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TerminalScreenSnapshot {
    pub cells: Vec<TerminalIndexedCellSnapshot>,
    pub lines: Vec<Vec<TerminalCellSnapshot>>,
    pub cursor: Option<TerminalCursorSnapshot>,
    pub display_offset: usize,
    pub history_size: usize,
    pub total_lines: usize,
    pub rows: usize,
    pub cols: usize,
    pub mode: TerminalModeSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSearchMatch {
    pub buffer_line: usize,
    pub start_column: usize,
    pub end_column: usize,
    pub preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
                let clipped =
                    truncate_utf8_boundary(&data, MAX_TERMINAL_CLIPBOARD_BYTES).to_string();
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
        shell_integration_enabled: bool,
        scrolling_history: usize,
        runtime_state: Arc<RwLock<RuntimeState>>,
        debug_enabled: bool,
    ) -> Result<Self, String> {
        let session_id = session_id.into();
        let backend = TerminalBackend::PortablePtyFeedingAlacritty;

        let candidates = shell_candidates(preferred_terminal.as_ref(), shell_integration_enabled);
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

    pub fn scroll_to_display_offset(&self, display_offset: usize) -> Result<(), String> {
        let (clamped_offset, changed) = {
            let mut term = self
                .term
                .lock()
                .map_err(|_| "Terminal state poisoned".to_string())?;
            let history_size = term
                .grid()
                .total_lines()
                .saturating_sub(term.grid().screen_lines());
            let target = display_offset.min(history_size);
            let current = term.grid().display_offset();
            if current == target {
                return Ok(());
            }
            let delta = target as i32 - current as i32;
            term.scroll_display(Scroll::Delta(delta));
            (term.grid().display_offset(), true)
        };

        if changed {
            if let Ok(mut runtime) = self.runtime_state.write() {
                if let Some(session) = runtime.sessions.get_mut(&self.session_id) {
                    session.note_scroll(clamped_offset);
                }
            }
        }

        Ok(())
    }

    pub fn scroll_to_buffer_line(&self, buffer_line: usize) -> Result<(), String> {
        let target_offset = {
            let term = self
                .term
                .lock()
                .map_err(|_| "Terminal state poisoned".to_string())?;
            let history_size = term
                .grid()
                .total_lines()
                .saturating_sub(term.grid().screen_lines());
            let total_lines = term.grid().total_lines().max(1);
            let screen_lines = term.screen_lines().max(1);
            let clamped_line = buffer_line.min(total_lines.saturating_sub(1));
            let grid_line = clamped_line as i32 - history_size as i32;
            let desired_viewport_row = screen_lines as i32 / 2;
            let target = desired_viewport_row.saturating_sub(grid_line).max(0) as usize;
            target.min(history_size)
        };

        self.scroll_to_display_offset(target_offset)
    }

    pub fn screen_text(&self) -> String {
        let term = match self.term.lock() {
            Ok(term) => term,
            Err(error) => error.into_inner(),
        };
        terminal_buffer_lines(&term)
            .into_iter()
            .skip(
                term.grid()
                    .total_lines()
                    .saturating_sub(term.screen_lines()),
            )
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn scrollback_text(&self) -> String {
        let term = match self.term.lock() {
            Ok(term) => term,
            Err(error) => error.into_inner(),
        };
        terminal_buffer_lines(&term).join("\n")
    }

    pub fn search(
        &self,
        query: &str,
        case_sensitive: bool,
        max_results: usize,
    ) -> Vec<TerminalSearchMatch> {
        let term = match self.term.lock() {
            Ok(term) => term,
            Err(error) => error.into_inner(),
        };
        search_terminal_buffer(&term, query, case_sensitive, max_results)
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

fn shell_candidates(
    preferred_terminal: Option<&DefaultTerminal>,
    shell_integration_enabled: bool,
) -> Vec<ShellCandidate> {
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
                    args: bash_shell_args(shell_integration_enabled),
                },
                ShellCandidate {
                    program: "bash".to_string(),
                    args: bash_shell_args(shell_integration_enabled),
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
                    args: bash_shell_args(shell_integration_enabled),
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

pub fn bash_shell_args(shell_integration_enabled: bool) -> Vec<String> {
    if shell_integration_enabled {
        let wrapper = crate::assets::ghostty_resources_dir()
            .join("shell-integration")
            .join("bash")
            .join("devmanager.bashrc");
        if wrapper.is_file() {
            return vec![
                "--rcfile".to_string(),
                wrapper.to_string_lossy().to_string(),
                "-i".to_string(),
            ];
        }
    }

    if cfg!(target_os = "windows") {
        vec!["--login".to_string()]
    } else {
        Vec::new()
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
    let total_lines = term.grid().total_lines();
    let history_size = total_lines.saturating_sub(rows);
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
        history_size,
        total_lines,
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
    env.entry("GHOSTTY_RESOURCES_DIR".to_string())
        .or_insert_with(|| {
            crate::assets::ghostty_resources_dir()
                .to_string_lossy()
                .to_string()
        });
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

mod cursor_shape_serde {
    use super::CursorShape;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(shape: &CursorShape, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(match shape {
            CursorShape::Block => "block",
            CursorShape::Underline => "underline",
            CursorShape::Beam => "beam",
            CursorShape::Hidden => "hidden",
            _ => "hidden",
        })
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<CursorShape, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "block" => CursorShape::Block,
            "underline" => CursorShape::Underline,
            "beam" => CursorShape::Beam,
            _ => CursorShape::Hidden,
        })
    }
}

fn write_system_clipboard_text(text: &str) -> Result<(), String> {
    let mut clipboard =
        Clipboard::new().map_err(|error| format!("Failed to open clipboard: {error}"))?;
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
        descendant_processes: Vec::new(),
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
        let mut shell_sequences = ShellSequenceParser::default();
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
                    let parsed_sequences = shell_sequences.push_chunk(&buffer[..bytes_read]);
                    let cursor_buffer_line = {
                        let mut term = match term.lock() {
                            Ok(term) => term,
                            Err(error) => error.into_inner(),
                        };
                        parser.advance(&mut *term, &buffer[..bytes_read]);
                        terminal_cursor_buffer_line(&term)
                    };

                    if let Some(writer) = log_writer.as_mut() {
                        writer.write_chunk(&buffer[..bytes_read]);
                    }

                    if let Ok(mut runtime) = runtime_state.write() {
                        if let Some(session) = runtime.sessions.get_mut(&session_id) {
                            session.record_pty_bytes(bytes_read);
                            session.note_output_activity();
                            apply_shell_sequences(session, &parsed_sequences, cursor_buffer_line);
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
            let surviving_descendants = pid
                .map(platform_service::collect_descendant_process_identities)
                .unwrap_or_default();
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
                let _ = pid_file::release_session_root(&session_id, pid, surviving_descendants);
            }
        }
        Err(error) => {
            if debug_enabled {
                eprintln!("[terminal:{session_id}] wait error: {error}");
            }
            let surviving_descendants = pid
                .map(platform_service::collect_descendant_process_identities)
                .unwrap_or_default();
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
                let _ = pid_file::release_session_root(&session_id, pid, surviving_descendants);
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

fn terminal_buffer_lines(term: &Term<SessionEventProxy>) -> Vec<String> {
    let grid = term.grid();
    let cols = term.columns();
    let history_size = grid.total_lines().saturating_sub(grid.screen_lines());
    let mut lines = Vec::with_capacity(history_size + term.screen_lines());

    for grid_line in -(history_size as i32)..(term.screen_lines() as i32) {
        let row = &grid[Line(grid_line)];
        let mut text = String::new();
        for cell in row.into_iter().take(cols) {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            text.push(renderable_char(cell));
            if let Some(extra) = cell.zerowidth() {
                for &character in extra {
                    text.push(character);
                }
            }
        }
        while text.ends_with(' ') {
            text.pop();
        }
        lines.push(text);
    }

    lines
}

fn search_terminal_buffer(
    term: &Term<SessionEventProxy>,
    query: &str,
    case_sensitive: bool,
    max_results: usize,
) -> Vec<TerminalSearchMatch> {
    let needle = query.trim();
    if needle.is_empty() || max_results == 0 {
        return Vec::new();
    }

    let mut matches = Vec::new();
    for (buffer_line, line) in terminal_buffer_lines(term).into_iter().enumerate() {
        let mut search_start = 0;
        while search_start <= line.len() {
            let Some(relative_start) =
                find_text_match(&line[search_start..], needle, case_sensitive)
            else {
                break;
            };
            let start = search_start + relative_start;
            let end = start + needle.len();
            matches.push(TerminalSearchMatch {
                buffer_line,
                start_column: start,
                end_column: end,
                preview: line.clone(),
            });
            if matches.len() >= max_results {
                return matches;
            }
            search_start = end.max(search_start + 1);
        }
    }

    matches
}

fn find_text_match(haystack: &str, needle: &str, case_sensitive: bool) -> Option<usize> {
    if case_sensitive {
        return haystack.find(needle);
    }

    if haystack.is_ascii() && needle.is_ascii() {
        for start in 0..=haystack.len().saturating_sub(needle.len()) {
            let end = start + needle.len();
            if haystack.get(start..end)?.eq_ignore_ascii_case(needle) {
                return Some(start);
            }
        }
        return None;
    }

    haystack.to_lowercase().find(&needle.to_lowercase())
}

#[derive(Debug)]
enum ShellSequence {
    PromptMark(PromptMarkKind, Option<i32>),
    ReportedCwd(PathBuf),
}

#[derive(Default)]
struct ShellSequenceParser {
    pending: Vec<u8>,
}

impl ShellSequenceParser {
    fn push_chunk(&mut self, chunk: &[u8]) -> Vec<ShellSequence> {
        self.pending.extend_from_slice(chunk);

        let mut events = Vec::new();
        let mut cursor = 0;
        let mut processed_until = 0;

        while cursor < self.pending.len() {
            if self.pending[cursor] == 0x1b
                && self
                    .pending
                    .get(cursor + 1)
                    .is_some_and(|byte| *byte == b']')
            {
                let start = cursor + 2;
                let Some((end, terminator_len)) = osc_terminator_bounds(&self.pending, start)
                else {
                    break;
                };

                if let Ok(payload) = std::str::from_utf8(&self.pending[start..end]) {
                    if let Some(event) = parse_shell_sequence(payload) {
                        events.push(event);
                    }
                }

                processed_until = end + terminator_len;
                cursor = processed_until;
                continue;
            }

            cursor += 1;
            processed_until = cursor;
        }

        if processed_until > 0 {
            self.pending.drain(0..processed_until);
        }
        if self.pending.len() > 8192 {
            let keep_from = self.pending.len().saturating_sub(1024);
            self.pending.drain(0..keep_from);
        }

        events
    }
}

fn osc_terminator_bounds(buffer: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut cursor = start;
    while cursor < buffer.len() {
        match buffer[cursor] {
            0x07 => return Some((cursor, 1)),
            0x1b if buffer.get(cursor + 1).is_some_and(|byte| *byte == b'\\') => {
                return Some((cursor, 2));
            }
            _ => cursor += 1,
        }
    }
    None
}

fn parse_shell_sequence(payload: &str) -> Option<ShellSequence> {
    if let Some(rest) = payload.strip_prefix("133;") {
        return parse_ghostty_prompt_mark(rest);
    }
    if let Some(rest) = payload.strip_prefix("7;") {
        return parse_ghostty_cwd(rest);
    }
    None
}

fn parse_ghostty_prompt_mark(payload: &str) -> Option<ShellSequence> {
    let mut parts = payload.split(';');
    let code = parts.next()?;
    match code {
        "A" => Some(ShellSequence::PromptMark(PromptMarkKind::PromptStart, None)),
        "P" => Some(ShellSequence::PromptMark(
            if payload.contains("k=s") {
                PromptMarkKind::PromptContinuation
            } else {
                PromptMarkKind::PromptStart
            },
            None,
        )),
        "B" => Some(ShellSequence::PromptMark(PromptMarkKind::InputReady, None)),
        "C" => Some(ShellSequence::PromptMark(
            PromptMarkKind::CommandStart,
            None,
        )),
        "D" => Some(ShellSequence::PromptMark(
            PromptMarkKind::CommandFinished,
            parts.next().and_then(|value| value.parse::<i32>().ok()),
        )),
        _ => None,
    }
}

fn parse_ghostty_cwd(payload: &str) -> Option<ShellSequence> {
    let url = payload
        .strip_prefix("kitty-shell-cwd://")
        .or_else(|| payload.strip_prefix("file://"))?;
    let slash = url.find('/')?;
    let decoded = percent_decode(&url[slash..]);
    let normalized = if cfg!(target_os = "windows")
        && decoded.len() > 3
        && decoded.starts_with('/')
        && decoded.as_bytes().get(2) == Some(&b':')
    {
        decoded[1..].to_string()
    } else {
        decoded
    };
    Some(ShellSequence::ReportedCwd(PathBuf::from(normalized)))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut cursor = 0;

    while cursor < bytes.len() {
        if bytes[cursor] == b'%' && cursor + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[cursor + 1..cursor + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    decoded.push(byte);
                    cursor += 3;
                    continue;
                }
            }
        }

        decoded.push(bytes[cursor]);
        cursor += 1;
    }

    String::from_utf8_lossy(&decoded).to_string()
}

fn terminal_cursor_buffer_line(term: &Term<SessionEventProxy>) -> usize {
    let content = term.renderable_content();
    let history_size = term
        .grid()
        .total_lines()
        .saturating_sub(term.grid().screen_lines());
    history_size.saturating_add(content.cursor.point.line.0.max(0) as usize)
}

fn apply_shell_sequences(
    session: &mut SessionRuntimeState,
    sequences: &[ShellSequence],
    buffer_line: usize,
) {
    if sequences.is_empty() {
        return;
    }

    session.note_shell_integration_detected(ShellIntegrationKind::Ghostty);
    for sequence in sequences {
        match sequence {
            ShellSequence::PromptMark(kind, exit_status) => {
                session.note_prompt_mark(buffer_line, *kind, *exit_status);
            }
            ShellSequence::ReportedCwd(cwd) => {
                session.note_shell_reported_cwd(cwd.clone());
            }
        }
    }
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
    fn search_terminal_buffer_finds_matches_across_scrollback() {
        let dimensions = SessionDimensions {
            cols: 32,
            rows: 2,
            cell_width: 8,
            cell_height: 16,
        };
        let proxy = test_event_proxy(dimensions);
        let mut term = Term::new(configured_term(1000), &TerminalSize::new(32, 2), proxy);
        let mut parser = Processor::<StdSyncHandler>::new();

        parser.advance(&mut term, b"alpha\r\nBeta alpha\r\ngamma\r\n");

        let matches = search_terminal_buffer(&term, "alpha", false, 8);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].buffer_line, 0);
        assert_eq!(matches[0].start_column, 0);
        assert_eq!(matches[1].buffer_line, 1);
        assert_eq!(matches[1].start_column, 5);
        assert_eq!(matches[1].preview, "Beta alpha");
    }

    #[test]
    fn shell_sequence_parser_handles_chunked_prompt_and_cwd_sequences() {
        let mut parser = ShellSequenceParser::default();

        let events = parser.push_chunk(b"\x1b]133;");
        assert!(events.is_empty());

        let events = parser.push_chunk(b"A\x07\x1b]7;file:///tmp/house%20hunter\x07");
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            ShellSequence::PromptMark(PromptMarkKind::PromptStart, None)
        ));
        assert!(matches!(
            &events[1],
            ShellSequence::ReportedCwd(path)
                if path == &PathBuf::from("/tmp/house hunter")
        ));
    }

    #[test]
    fn bash_shell_args_use_vendored_wrapper_when_enabled() {
        let args = bash_shell_args(true);

        assert_eq!(args.first().map(String::as_str), Some("--rcfile"));
        assert!(args.get(1).is_some_and(|value| value
            .ends_with("shell-integration\\bash\\devmanager.bashrc")
            || value.ends_with("shell-integration/bash/devmanager.bashrc")));
        assert_eq!(args.get(2).map(String::as_str), Some("-i"));
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
