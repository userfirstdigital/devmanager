use crate::models::DefaultTerminal;
use crate::services::pid_file;
use crate::state::{SessionDimensions, SessionExitState, SessionRuntimeState, SessionStatus};
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{point_to_viewport, Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, Processor, StdSyncHandler};
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use crate::state::RuntimeState;

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

#[derive(Debug, Clone, Default)]
pub struct TerminalScreenSnapshot {
    pub lines: Vec<String>,
    pub cursor: Option<TerminalCursorSnapshot>,
    pub display_offset: usize,
    pub rows: usize,
    pub cols: usize,
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
            Event::ClipboardLoad(_, _)
            | Event::ClipboardStore(_, _)
            | Event::ColorRequest(_, _) => {
                self.debug_log("ignored optional terminal event");
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
    backend: TerminalBackend,
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
                false,
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

    pub fn write_text(&self, text: &str) -> Result<(), String> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| "PTY writer poisoned".to_string())?;
        writer
            .write_all(text.as_bytes())
            .map_err(|error| format!("Failed to write to PTY: {error}"))?;
        writer
            .flush()
            .map_err(|error| format!("Failed to flush PTY input: {error}"))?;
        Ok(())
    }

    pub fn paste_text(&self, text: &str) -> Result<(), String> {
        let bracketed_paste = {
            let term = self
                .term
                .lock()
                .map_err(|_| "Terminal state poisoned".to_string())?;
            term.mode().contains(TermMode::BRACKETED_PASTE)
        };

        let normalized = text.replace("\r\n", "\n").replace('\n', "\r");
        if bracketed_paste {
            self.write_text(&format!("\u{1b}[200~{normalized}\u{1b}[201~"))
        } else {
            self.write_text(&normalized)
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

    pub fn snapshot(&self) -> TerminalScreenSnapshot {
        let term = match self.term.lock() {
            Ok(term) => term,
            Err(error) => error.into_inner(),
        };

        let content = term.renderable_content();
        let display_offset = content.display_offset;
        let rows = term.screen_lines();
        let cols = term.columns();
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

        let mut grid_lines = vec![vec!['\u{00a0}'; cols]; rows];
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

            grid_lines[point.line][point.column.0] = renderable_char(indexed.cell);
        }

        let lines = grid_lines
            .into_iter()
            .map(|line| line.into_iter().collect::<String>())
            .collect();

        TerminalScreenSnapshot {
            lines,
            cursor,
            display_offset,
            rows,
            cols,
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.close(false);
    }
}

#[derive(Clone)]
struct ShellCandidate {
    program: &'static str,
    args: Vec<String>,
}

fn shell_candidates(preferred_terminal: Option<&DefaultTerminal>) -> Vec<ShellCandidate> {
    if cfg!(target_os = "windows") {
        match preferred_terminal {
            Some(DefaultTerminal::Cmd) => vec![
                ShellCandidate {
                    program: "cmd",
                    args: Vec::new(),
                },
                ShellCandidate {
                    program: "pwsh",
                    args: vec!["-NoLogo".to_string()],
                },
                ShellCandidate {
                    program: "powershell",
                    args: vec!["-NoLogo".to_string()],
                },
            ],
            _ => vec![
                ShellCandidate {
                    program: "pwsh",
                    args: vec!["-NoLogo".to_string()],
                },
                ShellCandidate {
                    program: "powershell",
                    args: vec!["-NoLogo".to_string()],
                },
                ShellCandidate {
                    program: "cmd",
                    args: Vec::new(),
                },
            ],
        }
    } else {
        match preferred_terminal {
            Some(DefaultTerminal::Powershell) => vec![
                ShellCandidate {
                    program: "pwsh",
                    args: Vec::new(),
                },
                ShellCandidate {
                    program: "bash",
                    args: Vec::new(),
                },
                ShellCandidate {
                    program: "zsh",
                    args: Vec::new(),
                },
                ShellCandidate {
                    program: "sh",
                    args: Vec::new(),
                },
            ],
            _ => vec![
                ShellCandidate {
                    program: "bash",
                    args: Vec::new(),
                },
                ShellCandidate {
                    program: "zsh",
                    args: Vec::new(),
                },
                ShellCandidate {
                    program: "sh",
                    args: Vec::new(),
                },
                ShellCandidate {
                    program: "pwsh",
                    args: Vec::new(),
                },
            ],
        }
    }
}

fn renderable_char(cell: &Cell) -> char {
    if cell.flags.contains(Flags::HIDDEN) || cell.c == ' ' {
        '\u{00a0}'
    } else {
        cell.c
    }
}

fn configured_term(scrolling_history: usize) -> TermConfig {
    TermConfig {
        scrolling_history,
        ..Default::default()
    }
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
    mut log_file: Option<std::fs::File>,
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

                    if let Some(log_file) = log_file.as_mut() {
                        let _ = log_file.write_all(&buffer[..bytes_read]);
                        let _ = log_file.flush();
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
                pid_file::untrack_pid(pid);
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
                pid_file::untrack_pid(pid);
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

    let mut command = CommandBuilder::new(program.clone());
    if let Some(valid_cwd) = existing_directory(&cwd) {
        command.cwd(valid_cwd);
    }
    if !args.is_empty() {
        command.args(args.clone());
    }
    for (key, value) in env {
        command.env(key, value);
    }

    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("Failed to spawn command: {error}"))?;

    let pid = child.process_id();
    if track_pid {
        if let Some(pid) = pid {
            pid_file::track_pid(pid);
        }
    }
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| format!("Failed to acquire PTY writer: {error}"))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| format!("Failed to clone PTY reader: {error}"))?;
    let log_file = if let Some(log_file_path) = log_file_path {
        if let Some(parent) = log_file_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_file_path)
                .map_err(|error| {
                    format!(
                        "Failed to open log file {}: {error}",
                        log_file_path.display()
                    )
                })?,
        )
    } else {
        None
    };

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

    initialize_runtime_entry(
        &runtime_state,
        session_id,
        cwd.clone(),
        dimensions,
        program.clone(),
        backend,
        pid,
    );

    spawn_reader_thread(
        session_id.to_string(),
        reader,
        term.clone(),
        log_file,
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
        backend,
    })
}
