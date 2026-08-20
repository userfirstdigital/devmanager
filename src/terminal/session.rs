use crate::domain::id::{OperationId, ResourceId};
#[cfg(windows)]
use crate::domain::resource::ResourceKind;
use crate::models::{DefaultTerminal, MacTerminalProfile};
use crate::process::identity::ProcessOwner;
use crate::process::job::JobMemberObservation;
#[cfg(windows)]
use crate::process::launcher::{
    prepare_suspended_pty, validate_terminal_launch_source_bounds, LaunchIntent,
};
use crate::process::registry::ManagedProcessFence;
#[cfg(windows)]
use crate::process::teardown::{
    ManagedTerminalActorHandles, ManagedTerminalIo, ManagedTerminalTeardown,
};
use crate::process::teardown::{TeardownCompletionStore, MAX_MANAGED_TERMINAL_PORTS};
use crate::services::{pid_file, platform_service};
use crate::state::{
    PromptMarkKind, ResourceSnapshot, RuntimeState, SessionDimensions, SessionExitState,
    SessionKind, SessionRuntimeState, SessionStatus, ShellIntegrationKind,
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
use portable_pty::{native_pty_system, Child, MasterPty, PtySize, SlavePty};
#[cfg(not(windows))]
use portable_pty::{ChildKiller, CommandBuilder};
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use std::collections::BTreeMap;
use std::collections::HashMap;
#[cfg(windows)]
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::sync::MutexGuard;
#[cfg(windows)]
use std::sync::RwLockWriteGuard;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::Duration;

const MAX_TERMINAL_CLIPBOARD_BYTES: usize = 1024 * 1024;
const MAX_REMOTE_REPLAY_BYTES: usize = 4 * 1024 * 1024;
const MAX_TERMINAL_INPUT_BYTES: usize = 4 * 1024 * 1024;

#[cfg(all(test, windows))]
static FAIL_NEXT_WAIT_ACTOR_SPAWN: AtomicBool = AtomicBool::new(false);
#[cfg(all(test, windows))]
static FAIL_NEXT_INPUT_ADMISSION_OPEN: AtomicBool = AtomicBool::new(false);

/// Exact host-issued launch and teardown authority for one terminal runtime.
/// The terminal layer cannot invent Task ownership, resource generations, or
/// action epochs; the native host/task service must supply all of them.
#[derive(Clone)]
pub(crate) struct TerminalLaunchAuthority {
    owner: ProcessOwner,
    resource_id: ResourceId,
    runtime_generation: u64,
    operation_id: OperationId,
    action_epoch: u64,
    ports: Vec<u16>,
    completion_store: TeardownCompletionStore,
}

impl TerminalLaunchAuthority {
    pub(crate) fn new(
        owner: ProcessOwner,
        resource_id: ResourceId,
        runtime_generation: u64,
        operation_id: OperationId,
        action_epoch: u64,
        ports: Vec<u16>,
        completion_store: TeardownCompletionStore,
    ) -> Result<Self, String> {
        if runtime_generation == 0 || action_epoch == 0 {
            return Err("terminal launch generation and action epoch must be non-zero".to_string());
        }
        if ports.len() > MAX_MANAGED_TERMINAL_PORTS {
            return Err(format!(
                "terminal launch port set exceeds {MAX_MANAGED_TERMINAL_PORTS} entries"
            ));
        }
        let mut ports = ports;
        ports.sort_unstable();
        ports.dedup();
        Ok(Self {
            owner,
            resource_id,
            runtime_generation,
            operation_id,
            action_epoch,
            ports,
            completion_store,
        })
    }

    #[cfg(test)]
    pub(crate) fn identity_for_test(&self) -> (ProcessOwner, ResourceId, u64, u64) {
        (
            self.owner,
            self.resource_id,
            self.runtime_generation,
            self.action_epoch,
        )
    }
}

type SessionStateNotifier = Arc<dyn Fn() + Send + Sync>;
/// Shared PTY-output observer. Process-manager and terminal-service sinks both
/// observe the single reader; neither creates a second parser.
pub type TerminalOutputSink = Arc<dyn Fn(Vec<u8>, TerminalModeSnapshot) + Send + Sync>;
type SessionOutputNotifier = TerminalOutputSink;

/// Lifecycle observer for EOF / read failure / child exit on the one reader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalLifecycleEvent {
    ReaderEof,
    ReaderFailed { summary: String },
    ChildExited { summary: String, code: Option<u32> },
}

pub type TerminalLifecycleSink = Arc<dyn Fn(TerminalLifecycleEvent) + Send + Sync>;

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

#[derive(Clone)]
pub struct TerminalReplica {
    session_id: String,
    term: Arc<Mutex<Term<SessionEventProxy>>>,
    runtime_state: Arc<RwLock<RuntimeState>>,
    dimensions: Arc<Mutex<SessionDimensions>>,
    parser: Arc<Mutex<Processor<StdSyncHandler>>>,
    shell_sequences: Arc<Mutex<ShellSequenceParser>>,
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
    input_admission: Arc<AtomicBool>,
    runtime_state: Arc<RwLock<RuntimeState>>,
    dimensions: Arc<Mutex<SessionDimensions>>,
    debug_enabled: bool,
    state_notifier: Option<SessionStateNotifier>,
}

impl SessionEventProxy {
    fn write_to_pty(&self, text: &str) {
        let _ =
            write_composite_pty_payload(&self.writer, &self.input_admission, b"", text.as_bytes());
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

    fn notify_state_change(&self) {
        if let Some(notifier) = self.state_notifier.as_ref() {
            notifier();
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
        self.notify_state_change();
    }
}

#[cfg(not(windows))]
#[derive(Default)]
struct TerminalActorHandles {
    reader: Option<thread::JoinHandle<()>>,
    waiter: Option<thread::JoinHandle<()>>,
}

#[cfg(windows)]
type TerminalActorHandles = ManagedTerminalActorHandles;

#[derive(Default)]
struct TerminalActorStartGate {
    released: Mutex<bool>,
    changed: Condvar,
}

impl TerminalActorStartGate {
    fn wait(&self) {
        let mut released = self
            .released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !*released {
            released = self
                .changed
                .wait(released)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn release(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        self.changed.notify_all();
    }
}

pub struct TerminalSession {
    session_id: String,
    term: Arc<Mutex<Term<SessionEventProxy>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    input_admission: Arc<AtomicBool>,
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    actors: Arc<Mutex<TerminalActorHandles>>,
    #[cfg(not(windows))]
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    #[cfg(windows)]
    teardown: Arc<Mutex<Option<Arc<ManagedTerminalTeardown>>>>,
    #[cfg(windows)]
    lifecycle: Mutex<()>,
    #[cfg(windows)]
    retired: AtomicBool,
    #[cfg(all(test, windows))]
    managed_resource_publication_barrier: Mutex<Option<ManagedResourcePublicationTestBarrier>>,
    runtime_state: Arc<RwLock<RuntimeState>>,
    dimensions: Arc<Mutex<SessionDimensions>>,
    event_proxy: SessionEventProxy,
    backend: TerminalBackend,
    scrolling_history: Arc<RwLock<usize>>,
    replay_buffer: Arc<Mutex<Vec<u8>>>,
    output_notifier: Option<SessionOutputNotifier>,
    /// Extra observer for the host TerminalService. Shares the one PTY reader.
    service_output_sink: Arc<Mutex<Option<TerminalOutputSink>>>,
    /// Lifecycle observer for EOF/read-failure/child-exit on the one reader.
    service_lifecycle_sink: Arc<Mutex<Option<TerminalLifecycleSink>>>,
    /// Non-Windows attachment identity published by the launch authority.
    #[cfg(not(windows))]
    service_attachment_pin: Mutex<Option<(ResourceId, u64)>>,
}

#[cfg(all(test, windows))]
struct ManagedResourcePublicationTestBarrier {
    validated: std::sync::mpsc::SyncSender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

/// Opaque read-only result of one exact Job query. The retained teardown Arc
/// is deliberately private: accounting can carry this proof back to the
/// session for publication validation but cannot invoke teardown itself.
pub(crate) struct ManagedProcessObservationCapture {
    #[cfg(windows)]
    teardown: Arc<ManagedTerminalTeardown>,
    fence: ManagedProcessFence,
}

impl std::fmt::Debug for ManagedProcessObservationCapture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedProcessObservationCapture")
            .field("fence", &self.fence)
            .finish_non_exhaustive()
    }
}

impl ManagedProcessObservationCapture {
    pub(crate) fn fence(&self) -> &ManagedProcessFence {
        &self.fence
    }
}

#[derive(Debug)]
pub(crate) struct ManagedProcessObservationQuery {
    capture: ManagedProcessObservationCapture,
    members: Result<Vec<JobMemberObservation>, String>,
}

impl ManagedProcessObservationQuery {
    pub(crate) fn into_parts(
        self,
    ) -> (
        ManagedProcessObservationCapture,
        Result<Vec<JobMemberObservation>, String>,
    ) {
        (self.capture, self.members)
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedResourceSamplePublication {
    Published {
        dirty_changed: bool,
        cleared_unreaped: bool,
    },
    StaleGeneration {
        dirty_changed: bool,
    },
}

impl TerminalSession {
    pub(crate) fn spawn(
        session_id: impl Into<String>,
        cwd: PathBuf,
        dimensions: SessionDimensions,
        preferred_terminal: Option<DefaultTerminal>,
        mac_terminal_profile: Option<MacTerminalProfile>,
        shell_integration_enabled: bool,
        scrolling_history: usize,
        runtime_state: Arc<RwLock<RuntimeState>>,
        debug_enabled: bool,
        state_notifier: Option<SessionStateNotifier>,
        output_notifier: Option<SessionOutputNotifier>,
        authority: TerminalLaunchAuthority,
    ) -> Result<Self, String> {
        let session_id = session_id.into();
        let backend = TerminalBackend::PortablePtyFeedingAlacritty;

        let candidates = shell_candidates(
            preferred_terminal.as_ref(),
            mac_terminal_profile.as_ref(),
            shell_integration_enabled,
        );
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
                state_notifier.clone(),
                output_notifier.clone(),
                authority.clone(),
            ) {
                Ok(session) => return Ok(session),
                Err(error) => last_error = Some(format!("{}: {}", candidate.program, error)),
            }
        }

        Err(last_error.unwrap_or_else(|| "No shell candidate could be spawned".to_string()))
    }

    pub(crate) fn spawn_command(
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
        state_notifier: Option<SessionStateNotifier>,
        output_notifier: Option<SessionOutputNotifier>,
        authority: TerminalLaunchAuthority,
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
            state_notifier,
            output_notifier,
            authority,
        )
    }

    pub fn backend(&self) -> TerminalBackend {
        self.backend
    }

    pub fn id(&self) -> &str {
        &self.session_id
    }

    pub fn write_bytes(&self, bytes: &[u8]) -> Result<(), String> {
        write_composite_pty_payload(&self.writer, &self.input_admission, b"", bytes)
    }

    /// Queue one provider-automation key boundary without flushing the
    /// ConPTY writer. SessionStart hooks must return before the provider TUI
    /// can read input; flushing from the host during that hook can otherwise
    /// deadlock the hook acknowledgement against the provider reader.
    pub(crate) fn write_provider_bytes(&self, bytes: &[u8]) -> Result<(), String> {
        write_composite_pty_payload_inner(&self.writer, &self.input_admission, b"", bytes, false)
    }

    /// Install or replace the TerminalService observer on the existing reader.
    /// Does not spawn a second reader or parser; process-manager notifiers stay.
    pub fn install_service_output_sink(&self, sink: TerminalOutputSink) -> Result<(), String> {
        let mut slot = self
            .service_output_sink
            .lock()
            .map_err(|_| "terminal service output sink poisoned".to_string())?;
        *slot = Some(sink);
        Ok(())
    }

    pub fn clear_service_output_sink(&self) -> Result<(), String> {
        let mut slot = self
            .service_output_sink
            .lock()
            .map_err(|_| "terminal service output sink poisoned".to_string())?;
        *slot = None;
        Ok(())
    }

    pub fn install_service_lifecycle_sink(
        &self,
        sink: TerminalLifecycleSink,
    ) -> Result<(), String> {
        let mut slot = self
            .service_lifecycle_sink
            .lock()
            .map_err(|_| "terminal service lifecycle sink poisoned".to_string())?;
        *slot = Some(sink);
        Ok(())
    }

    pub fn clear_service_lifecycle_sink(&self) -> Result<(), String> {
        let mut slot = self
            .service_lifecycle_sink
            .lock()
            .map_err(|_| "terminal service lifecycle sink poisoned".to_string())?;
        *slot = None;
        Ok(())
    }

    /// Current typed attachment fence. Windows uses the managed process fence;
    /// non-Windows uses the generation published by the launch authority.
    pub fn current_attachment_fence(&self) -> Result<(ResourceId, u64), String> {
        #[cfg(windows)]
        {
            let fence = self
                .managed_process_fence()?
                .ok_or_else(|| "managed terminal fence is missing".to_string())?;
            Ok((
                fence.resource().resource_id,
                fence.resource().runtime_generation,
            ))
        }
        #[cfg(not(windows))]
        {
            let pin = self
                .service_attachment_pin
                .lock()
                .map_err(|_| "terminal attachment pin poisoned".to_string())?;
            (*pin).ok_or_else(|| "terminal attachment fence is missing".to_string())
        }
    }

    /// Exact generation-fenced teardown for the host TerminalService.
    /// Requires the caller-supplied expected fence; never closes a newer or
    /// missing generation. Non-Windows remains fail-closed on the same check.
    pub fn close_exact_for_service(
        &self,
        expected_resource_id: ResourceId,
        expected_generation: u64,
        closed_by_user: bool,
    ) -> Result<(), String> {
        let (current_resource_id, current_generation) = self.current_attachment_fence()?;
        if current_resource_id != expected_resource_id || current_generation != expected_generation
        {
            return Err(
                "managed terminal generation changed before teardown admission".to_string(),
            );
        }
        #[cfg(windows)]
        {
            let fence = self
                .managed_process_fence()?
                .ok_or_else(|| "managed terminal fence is missing".to_string())?;
            if fence.resource().resource_id != expected_resource_id
                || fence.resource().runtime_generation != expected_generation
            {
                return Err(
                    "managed terminal generation changed before teardown admission".to_string(),
                );
            }
            self.close_managed_process_exact(&fence, closed_by_user)
        }
        #[cfg(not(windows))]
        {
            self.close(closed_by_user)
        }
    }

    /// Bound host scrollback to exactly `max_lines` (minimum 1). Unlike
    /// `set_scrollback_lines`, this does not clamp upward to 100.
    pub fn bound_history_exact(&self, max_lines: usize) {
        let max_lines = max_lines.max(1);
        if let Ok(mut scrollback) = self.scrolling_history.write() {
            *scrollback = max_lines;
        }
        if let Ok(mut term) = self.term.lock() {
            term.set_options(configured_term(max_lines));
        }
        self.event_proxy.with_runtime(|session| {
            session.display_offset = session.display_offset.min(max_lines);
            session.mark_dirty();
        });
    }

    pub fn session_view(&self) -> Option<TerminalSessionView> {
        let runtime = self
            .runtime_state
            .read()
            .ok()
            .and_then(|runtime| runtime.sessions.get(&self.session_id).cloned())?;
        Some(TerminalSessionView {
            runtime,
            screen: self.snapshot(),
        })
    }

    pub fn write_text(&self, text: &str) -> Result<(), String> {
        self.write_bytes(text.as_bytes())
    }

    pub fn paste_text(&self, text: &str) -> Result<(), String> {
        validate_terminal_input_source_bounds(b"", text.as_bytes())?;
        let bracketed_paste = {
            let term = self
                .term
                .lock()
                .map_err(|_| "Terminal state poisoned".to_string())?;
            term.mode().contains(TermMode::BRACKETED_PASTE)
        };

        let payload = prepare_paste_payload("", text, bracketed_paste)?;
        write_composite_pty_payload(&self.writer, &self.input_admission, b"", &payload)
    }

    /// Writes a user-origin text boundary and its DevManager prefix as one PTY
    /// payload. Callers commit attachment delivery only after this succeeds.
    pub fn write_user_text(&self, prefix: &str, text: &str) -> Result<(), String> {
        write_composite_pty_payload(
            &self.writer,
            &self.input_admission,
            prefix.as_bytes(),
            text.as_bytes(),
        )
    }

    /// Writes a user-origin raw byte boundary and its DevManager prefix as one
    /// PTY payload.
    pub fn write_user_bytes(&self, prefix: &str, bytes: &[u8]) -> Result<(), String> {
        write_composite_pty_payload(
            &self.writer,
            &self.input_admission,
            prefix.as_bytes(),
            bytes,
        )
    }

    /// Pastes user-origin text while keeping the DevManager prefix outside the
    /// terminal's bracketed-paste markers.
    pub fn paste_user_text(&self, prefix: &str, text: &str) -> Result<(), String> {
        validate_terminal_input_source_bounds(prefix.as_bytes(), text.as_bytes())?;
        let bracketed_paste = {
            let term = self
                .term
                .lock()
                .map_err(|_| "Terminal state poisoned".to_string())?;
            term.mode().contains(TermMode::BRACKETED_PASTE)
        };
        let payload = prepare_paste_payload(prefix, text, bracketed_paste)?;
        write_composite_pty_payload(&self.writer, &self.input_admission, b"", &payload)
    }

    pub fn resize(&self, dimensions: SessionDimensions) -> Result<(), String> {
        {
            let mut master = self
                .master
                .lock()
                .map_err(|_| "PTY master poisoned".to_string())?;
            master
                .as_mut()
                .ok_or_else(|| "PTY master is closed".to_string())?
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
        self.event_proxy.notify_state_change();

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
        self.event_proxy.notify_state_change();

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
            self.event_proxy.notify_state_change();
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
        #[cfg(windows)]
        {
            let _lifecycle = lock_terminal_lifecycle(&self.lifecycle)?;
            self.close_managed_current(closed_by_user)
        }
        #[cfg(not(windows))]
        {
            self.note_close_requested(closed_by_user);
            self.sync_tracked_descendants();
            let killer_deadline = std::time::Instant::now()
                .checked_add(terminal_actor_cancellation_timeout())
                .ok_or_else(|| "terminal killer cancellation deadline overflow".to_string())?;
            // Revoke admission while holding the writer lock before asking
            // the child capability to terminate. Writers recheck admission
            // under this same lock, so none can slip through between teardown
            // initiation and process termination.
            drain_terminal_input_until(&self.input_admission, &self.writer, killer_deadline)?;
            let kill_result = lock_terminal_actor_resource_until(
                &self.killer,
                killer_deadline,
                "terminal session killer",
            )?
            .kill();
            if let Err(error) = kill_result {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(format!("Failed to terminate shell session: {error}"));
                }
            }
            self.detach_pty_and_join_actors()
        }
    }

    fn note_close_requested(&self, closed_by_user: bool) {
        // Runtime status is a projection, never process authority. Do not let
        // a contended UI/background read delay the exact Job teardown path.
        if let Ok(mut runtime) = self.runtime_state.try_write() {
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
    }

    #[cfg(windows)]
    fn close_managed_teardown(teardown: &ManagedTerminalTeardown) -> Result<(), String> {
        let report = teardown.close()?;
        if report.outcome() == crate::process::teardown::TeardownOutcome::Closed {
            Ok(())
        } else {
            Err(format!(
                "Terminal teardown did not close: {:?}; errors: {:?}",
                report.outcome(),
                report.errors()
            ))
        }
    }

    #[cfg(windows)]
    fn close_managed_current(&self, closed_by_user: bool) -> Result<(), String> {
        self.note_close_requested(closed_by_user);
        let teardown = lock_terminal_teardown_slot(&self.teardown)?
            .clone()
            .ok_or_else(|| "Managed terminal teardown authority is missing".to_string())?;
        Self::close_managed_teardown(&teardown)
    }

    /// Closes only the exact terminal generation selected by the caller. A
    /// concurrent restart can replace the session slot, but can never cause a
    /// stale diagnostic action to terminate that replacement.
    #[cfg(windows)]
    pub(crate) fn close_managed_process_exact(
        &self,
        expected: &ManagedProcessFence,
        closed_by_user: bool,
    ) -> Result<(), String> {
        let _lifecycle = lock_terminal_lifecycle(&self.lifecycle)?;
        let teardown = lock_terminal_teardown_slot(&self.teardown)?
            .clone()
            .ok_or_else(|| "Managed terminal teardown authority is missing".to_string())?;
        if !teardown.matches_fence(expected) {
            return Err(
                "Managed terminal generation changed before teardown admission".to_string(),
            );
        }
        if self.retired.load(Ordering::Acquire) {
            return Ok(());
        }
        self.note_close_requested(closed_by_user);
        Self::close_managed_teardown(&teardown)?;
        self.retired.store(true, Ordering::Release);
        Ok(())
    }

    #[cfg(not(windows))]
    fn detach_pty_and_join_actors(&self) -> Result<(), String> {
        detach_pty_and_join_actor_slots(
            &self.input_admission,
            &self.writer,
            &self.master,
            &self.actors,
        )
    }

    #[cfg(test)]
    fn live_actor_count_for_test(&self) -> usize {
        self.actors
            .lock()
            .map(|actors| {
                usize::from(actors.reader.is_some()) + usize::from(actors.waiter.is_some())
            })
            .unwrap_or(usize::MAX)
    }

    pub(crate) fn restart_command(
        &self,
        cwd: PathBuf,
        dimensions: SessionDimensions,
        program: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        log_file_path: Option<PathBuf>,
        track_pid: bool,
        authority: TerminalLaunchAuthority,
    ) -> Result<(), String> {
        #[cfg(not(windows))]
        self.validate_service_restart_fence(&authority)?;
        #[cfg(windows)]
        let _lifecycle = lock_terminal_lifecycle(&self.lifecycle)?;
        #[cfg(windows)]
        if self.retired.load(Ordering::Acquire) {
            return Err("Managed terminal generation is retired".to_string());
        }
        #[cfg(windows)]
        self.close_managed_current(false)?;
        #[cfg(not(windows))]
        self.close(false)?;

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(pty_size(dimensions))
            .map_err(|error| error.to_string())?;
        #[cfg(windows)]
        let io = ManagedTerminalIo::new(
            Arc::clone(&self.writer),
            Arc::clone(&self.master),
            Arc::clone(&self.actors),
            Arc::clone(&self.input_admission),
        );
        #[cfg(windows)]
        let (child, teardown) = spawn_suspended_managed_terminal(
            &*pair.slave,
            &self.session_id,
            &cwd,
            &program,
            &args,
            env,
            authority,
            Arc::clone(&io),
        )?;
        #[cfg(not(windows))]
        let child = {
            let mut command = CommandBuilder::new(program.clone());
            if let Some(valid_cwd) = existing_directory(&cwd) {
                command.cwd(valid_cwd);
            }
            if !args.is_empty() {
                command.args(args.clone());
            }
            apply_terminal_env_defaults(&mut command, env);
            pair.slave
                .spawn_command(command)
                .map_err(|error| format!("Failed to spawn command: {error}"))?
        };

        let pid = child.process_id();
        #[cfg(not(windows))]
        let mut cleanup_killer = child.clone_killer();
        #[cfg(windows)]
        let teardown_handle = Arc::new(Mutex::new(Some(teardown.clone())));
        #[cfg(not(windows))]
        let mut process_job = attach_managed_process_job(pid);

        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(error) => {
                cleanup_failed_spawn(
                    #[cfg(not(windows))]
                    &mut cleanup_killer,
                    #[cfg(windows)]
                    &teardown,
                );
                return Err(format!("Failed to acquire PTY writer: {error}"));
            }
        };
        let reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(error) => {
                cleanup_failed_spawn(
                    #[cfg(not(windows))]
                    &mut cleanup_killer,
                    #[cfg(windows)]
                    &teardown,
                );
                return Err(format!("Failed to clone PTY reader: {error}"));
            }
        };
        let log_writer = open_log_writer(log_file_path);

        {
            let mut writer_slot = match self.writer.lock() {
                Ok(writer_slot) => writer_slot,
                Err(_) => {
                    cleanup_failed_spawn(
                        #[cfg(not(windows))]
                        &mut cleanup_killer,
                        #[cfg(windows)]
                        &teardown,
                    );
                    return Err("PTY writer poisoned".to_string());
                }
            };
            *writer_slot = writer;
        }
        {
            let mut master_slot = match self.master.lock() {
                Ok(master_slot) => master_slot,
                Err(_) => {
                    cleanup_failed_spawn(
                        #[cfg(not(windows))]
                        &mut cleanup_killer,
                        #[cfg(windows)]
                        &teardown,
                    );
                    return Err("PTY master poisoned".to_string());
                }
            };
            *master_slot = Some(pair.master);
        }
        {
            #[cfg(not(windows))]
            let mut killer_slot = match self.killer.lock() {
                Ok(killer_slot) => killer_slot,
                Err(_) => {
                    cleanup_failed_spawn(
                        #[cfg(not(windows))]
                        &mut cleanup_killer,
                        #[cfg(windows)]
                        &teardown,
                    );
                    #[cfg(not(windows))]
                    drop(process_job.take());
                    return Err("Session killer poisoned".to_string());
                }
            };
            #[cfg(not(windows))]
            {
                *killer_slot = child.clone_killer();
            }
        }
        {
            let mut current_dimensions = match self.dimensions.lock() {
                Ok(current_dimensions) => current_dimensions,
                Err(_) => {
                    cleanup_failed_spawn(
                        #[cfg(not(windows))]
                        &mut cleanup_killer,
                        #[cfg(windows)]
                        &teardown,
                    );
                    return Err("Size lock poisoned".to_string());
                }
            };
            *current_dimensions = dimensions;
        }
        {
            let mut term = match self.term.lock() {
                Ok(term) => term,
                Err(_) => {
                    cleanup_failed_spawn(
                        #[cfg(not(windows))]
                        &mut cleanup_killer,
                        #[cfg(windows)]
                        &teardown,
                    );
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
                cleanup_failed_spawn(
                    #[cfg(not(windows))]
                    &mut cleanup_killer,
                    #[cfg(windows)]
                    &teardown,
                );
                return Err(error);
            }
        }

        if let Ok(mut replay) = self.replay_buffer.lock() {
            replay.clear();
        }
        #[cfg(not(windows))]
        {
            let mut job_slot = match self.process_job.lock() {
                Ok(job_slot) => job_slot,
                Err(_) => {
                    cleanup_failed_spawn(
                        #[cfg(not(windows))]
                        &mut cleanup_killer,
                        #[cfg(windows)]
                        &teardown,
                    );
                    drop(process_job.take());
                    return Err("Process job poisoned".to_string());
                }
            };
            *job_slot = process_job.take();
        }
        #[cfg(windows)]
        {
            let mut teardown_slot = lock_terminal_teardown_slot(&self.teardown).map_err(|_| {
                cleanup_failed_spawn(&teardown);
                "Session teardown poisoned".to_string()
            })?;
            *teardown_slot = teardown_handle
                .lock()
                .map_err(|_| {
                    cleanup_failed_spawn(&teardown);
                    "Session teardown poisoned".to_string()
                })?
                .clone();
        }

        let mut actor_slots = match self.actors.lock() {
            Ok(actor_slots) => actor_slots,
            Err(_) => {
                cleanup_failed_spawn(
                    #[cfg(not(windows))]
                    &mut cleanup_killer,
                    #[cfg(windows)]
                    &teardown,
                );
                return Err("Terminal actor handles poisoned".to_string());
            }
        };
        if actor_slots.reader.is_some() || actor_slots.waiter.is_some() {
            cleanup_failed_spawn(
                #[cfg(not(windows))]
                &mut cleanup_killer,
                #[cfg(windows)]
                &teardown,
            );
            return Err("previous terminal actors were not joined before restart".to_string());
        }
        let start_gate = Arc::new(TerminalActorStartGate::default());
        let reader_actor = match spawn_reader_thread(
            self.session_id.clone(),
            reader,
            self.term.clone(),
            log_writer,
            self.runtime_state.clone(),
            self.event_proxy.debug_enabled,
            self.event_proxy.state_notifier.clone(),
            self.output_notifier.clone(),
            Arc::clone(&self.service_output_sink),
            Arc::clone(&self.service_lifecycle_sink),
            self.replay_buffer.clone(),
            Arc::clone(&start_gate),
            #[cfg(windows)]
            teardown_handle.clone(),
        ) {
            Ok(actor) => actor,
            Err(error) => {
                drop(actor_slots);
                cleanup_failed_spawn(
                    #[cfg(not(windows))]
                    &mut cleanup_killer,
                    #[cfg(windows)]
                    &teardown,
                );
                return Err(error);
            }
        };
        actor_slots.reader = Some(reader_actor);
        let wait_actor = match spawn_wait_thread(
            self.session_id.clone(),
            child,
            pid,
            #[cfg(windows)]
            teardown_handle,
            #[cfg(not(windows))]
            self.process_job.clone(),
            self.runtime_state.clone(),
            self.event_proxy.debug_enabled,
            self.event_proxy.state_notifier.clone(),
            Arc::clone(&self.service_lifecycle_sink),
            Arc::clone(&start_gate),
        ) {
            Ok(actor) => actor,
            Err(error) => {
                drop(actor_slots);
                start_gate.release();
                cleanup_failed_spawn(
                    #[cfg(not(windows))]
                    &mut cleanup_killer,
                    #[cfg(windows)]
                    &teardown,
                );
                #[cfg(not(windows))]
                if self.detach_pty_and_join_actors().is_err() {
                    std::process::abort();
                }
                return Err(error);
            }
        };
        actor_slots.waiter = Some(wait_actor);
        drop(actor_slots);
        start_gate.release();
        #[cfg(windows)]
        if let Err(error) = open_managed_terminal_input(&io) {
            cleanup_failed_spawn(&teardown);
            return Err(error);
        }
        #[cfg(not(windows))]
        self.input_admission.store(true, Ordering::Release);

        #[cfg(not(windows))]
        self.publish_service_attachment_fence(&authority)?;

        self.event_proxy.debug_log(format!("respawned {}", program));
        Ok(())
    }

    #[cfg(not(windows))]
    fn sync_tracked_descendants(&self) {
        let root_pid = self.runtime_state.read().ok().and_then(|runtime| {
            runtime
                .sessions
                .get(&self.session_id)
                .and_then(|session| session.pid)
        });
        if let Some(root_pid) = root_pid {
            let descendants = platform_service::collect_descendant_process_identities(root_pid);
            let _ = pid_file::sync_session_descendant_processes(
                &self.session_id,
                root_pid,
                descendants,
            );
        }
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

    pub fn replay_bytes(&self) -> Vec<u8> {
        self.replay_buffer
            .lock()
            .map(|buffer| buffer.clone())
            .unwrap_or_default()
    }
}

impl TerminalReplica {
    pub fn from_bootstrap(
        session_id: impl Into<String>,
        runtime: SessionRuntimeState,
        replay_bytes: &[u8],
    ) -> Self {
        let session_id = session_id.into();
        let runtime_state = Arc::new(RwLock::new(RuntimeState::default()));
        if let Ok(mut runtime_slot) = runtime_state.write() {
            runtime_slot
                .sessions
                .insert(session_id.clone(), runtime.clone());
        }
        let dimensions = Arc::new(Mutex::new(runtime.dimensions));
        let event_proxy = SessionEventProxy {
            session_id: session_id.clone(),
            writer: Arc::new(Mutex::new(
                Box::new(std::io::sink()) as Box<dyn Write + Send>
            )),
            input_admission: Arc::new(AtomicBool::new(false)),
            runtime_state: runtime_state.clone(),
            dimensions: dimensions.clone(),
            debug_enabled: false,
            state_notifier: None,
        };
        let term = Arc::new(Mutex::new(Term::new(
            configured_term(10_000),
            &TerminalSize::new(
                runtime.dimensions.cols as usize,
                runtime.dimensions.rows as usize,
            ),
            event_proxy,
        )));
        let replica = Self {
            session_id: session_id.clone(),
            term,
            runtime_state,
            dimensions,
            parser: Arc::new(Mutex::new(Processor::<StdSyncHandler>::new())),
            shell_sequences: Arc::new(Mutex::new(ShellSequenceParser::default())),
        };
        if !replay_bytes.is_empty() {
            replica.apply_output_bytes(replay_bytes);
        }
        replica.apply_runtime(runtime);
        replica
    }

    pub fn apply_output_bytes(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let Ok(mut parser) = self.parser.lock() else {
            return;
        };
        let Ok(mut shell_sequences) = self.shell_sequences.lock() else {
            return;
        };
        apply_terminal_output_chunk(
            &self.session_id,
            bytes,
            &self.term,
            &mut parser,
            &mut shell_sequences,
            &self.runtime_state,
        );
    }

    pub fn apply_runtime(&self, runtime: SessionRuntimeState) {
        if let Ok(mut dimensions) = self.dimensions.lock() {
            *dimensions = runtime.dimensions;
        }
        if let Ok(mut term) = self.term.lock() {
            term.resize(TerminalSize::new(
                runtime.dimensions.cols as usize,
                runtime.dimensions.rows as usize,
            ));
            apply_display_offset_to_term(&mut term, runtime.display_offset);
        }
        if let Ok(mut runtime_state) = self.runtime_state.write() {
            runtime_state
                .sessions
                .insert(self.session_id.clone(), runtime.clone());
            runtime_state.active_session_id = Some(self.session_id.clone());
        }
    }

    pub fn apply_local_resize(&self, dimensions: SessionDimensions) {
        if let Ok(mut dimensions_slot) = self.dimensions.lock() {
            *dimensions_slot = dimensions;
        }

        let display_offset = self
            .runtime_state
            .read()
            .ok()
            .and_then(|runtime_state| {
                runtime_state
                    .sessions
                    .get(&self.session_id)
                    .map(|session| session.display_offset)
            })
            .unwrap_or(0);

        if let Ok(mut term) = self.term.lock() {
            term.resize(TerminalSize::new(
                dimensions.cols as usize,
                dimensions.rows as usize,
            ));
            apply_display_offset_to_term(&mut term, display_offset);
        }

        if let Ok(mut runtime_state) = self.runtime_state.write() {
            if let Some(session) = runtime_state.sessions.get_mut(&self.session_id) {
                session.dimensions = dimensions;
            }
            runtime_state.active_session_id = Some(self.session_id.clone());
        }
    }

    pub fn view(&self) -> Option<TerminalSessionView> {
        let runtime = self
            .runtime_state
            .read()
            .ok()
            .and_then(|runtime| runtime.sessions.get(&self.session_id).cloned())?;
        let screen = self.term.lock().ok().map(|term| snapshot_term(&term))?;
        Some(TerminalSessionView { runtime, screen })
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

    /// Bound replica scrollback to the exact host-admitted limit.
    pub fn bound_history_exact(&self, max_lines: usize) {
        let max_lines = max_lines.max(1);
        let mut term = match self.term.lock() {
            Ok(term) => term,
            Err(error) => error.into_inner(),
        };
        term.set_options(configured_term(max_lines));
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
        if let Err(error) = self.close(false) {
            eprintln!(
                "terminal session `{}` dropped before bounded teardown completed: {error}",
                self.session_id
            );
            // A live actor cannot be detached safely. The process may still
            // own PTY resources, so every platform must fail closed here.
            std::process::abort();
        }
    }
}

#[derive(Clone)]
struct ShellCandidate {
    program: String,
    args: Vec<String>,
}

fn shell_candidates(
    preferred_terminal: Option<&DefaultTerminal>,
    mac_profile: Option<&MacTerminalProfile>,
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
            DefaultTerminal::Powershell | DefaultTerminal::Pwsh => vec![
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
                    args: bash_shell_args(shell_integration_enabled),
                },
                ShellCandidate {
                    program: "zsh".to_string(),
                    args: vec!["-l".to_string()],
                },
                ShellCandidate {
                    program: "sh".to_string(),
                    args: vec!["-l".to_string()],
                },
            ],
            _ => {
                let prefer_zsh = if cfg!(target_os = "macos") {
                    match mac_profile {
                        Some(MacTerminalProfile::Bash) => false,
                        Some(MacTerminalProfile::Zsh) => true,
                        _ => {
                            // System default: check $SHELL, default to zsh
                            // (macOS ships bash 3.2; zsh is the modern default)
                            !std::env::var("SHELL")
                                .unwrap_or_default()
                                .ends_with("/bash")
                        }
                    }
                } else {
                    false
                };

                if prefer_zsh {
                    vec![
                        ShellCandidate {
                            program: "zsh".to_string(),
                            args: vec!["-l".to_string()],
                        },
                        ShellCandidate {
                            program: "bash".to_string(),
                            args: bash_shell_args(shell_integration_enabled),
                        },
                        ShellCandidate {
                            program: "sh".to_string(),
                            args: vec!["-l".to_string()],
                        },
                        ShellCandidate {
                            program: "pwsh".to_string(),
                            args: Vec::new(),
                        },
                    ]
                } else {
                    vec![
                        ShellCandidate {
                            program: "bash".to_string(),
                            args: bash_shell_args(shell_integration_enabled),
                        },
                        ShellCandidate {
                            program: "zsh".to_string(),
                            args: vec!["-l".to_string()],
                        },
                        ShellCandidate {
                            program: "sh".to_string(),
                            args: vec!["-l".to_string()],
                        },
                        ShellCandidate {
                            program: "pwsh".to_string(),
                            args: Vec::new(),
                        },
                    ]
                }
            }
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

    vec!["--login".to_string()]
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

#[cfg(not(windows))]
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

fn composite_paste_payload(prefix: &str, text: &str, bracketed_paste: bool) -> Vec<u8> {
    let pasted = if bracketed_paste {
        format!(
            "\u{1b}[200~{}\u{1b}[201~",
            sanitize_bracketed_paste_text(text)
        )
    } else {
        normalize_plain_paste_text(text)
    };
    let mut payload = Vec::with_capacity(prefix.len().saturating_add(pasted.len()));
    payload.extend_from_slice(prefix.as_bytes());
    payload.extend_from_slice(pasted.as_bytes());
    payload
}

fn prepare_paste_payload(
    prefix: &str,
    text: &str,
    bracketed_paste: bool,
) -> Result<Vec<u8>, String> {
    validate_terminal_input_source_bounds(prefix.as_bytes(), text.as_bytes())?;
    Ok(composite_paste_payload(prefix, text, bracketed_paste))
}

fn validate_terminal_input_source_bounds(prefix: &[u8], input: &[u8]) -> Result<(), String> {
    let total = prefix
        .len()
        .checked_add(input.len())
        .ok_or_else(|| "PTY input byte count overflow".to_string())?;
    if total > MAX_TERMINAL_INPUT_BYTES {
        return Err(format!(
            "PTY input exceeds {MAX_TERMINAL_INPUT_BYTES} bytes"
        ));
    }
    Ok(())
}

fn write_composite_pty_payload(
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    input_admission: &AtomicBool,
    prefix: &[u8],
    input: &[u8],
) -> Result<(), String> {
    write_composite_pty_payload_inner(writer, input_admission, prefix, input, true)
}

fn write_composite_pty_payload_inner(
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    input_admission: &AtomicBool,
    prefix: &[u8],
    input: &[u8],
    flush: bool,
) -> Result<(), String> {
    validate_terminal_input_source_bounds(prefix, input)?;
    let total = prefix.len() + input.len();
    let mut writer = writer
        .lock()
        .map_err(|_| "PTY writer poisoned".to_string())?;
    if !input_admission.load(Ordering::Acquire) {
        return Err("PTY input is closed for terminal teardown".to_string());
    }
    let mut payload = Vec::with_capacity(total);
    payload.extend_from_slice(prefix);
    payload.extend_from_slice(input);
    writer
        .write_all(&payload)
        .map_err(|error| format!("Failed to write to PTY: {error}"))?;
    if flush {
        writer
            .flush()
            .map_err(|error| format!("Failed to flush PTY input: {error}"))?;
    }
    Ok(())
}

fn drain_terminal_input_until(
    input_admission: &AtomicBool,
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    absolute_deadline: std::time::Instant,
) -> Result<(), String> {
    loop {
        if std::time::Instant::now() >= absolute_deadline {
            return Err("terminal input drain exceeded its absolute deadline".to_string());
        }
        match writer.try_lock() {
            Ok(_writer) => {
                input_admission.store(false, Ordering::Release);
                if std::time::Instant::now() >= absolute_deadline {
                    return Err("terminal input drain exceeded its absolute deadline".to_string());
                }
                return Ok(());
            }
            Err(std::sync::TryLockError::WouldBlock) => thread::yield_now(),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("terminal PTY writer poisoned".to_string())
            }
        }
    }
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

#[cfg(windows)]
fn resolve_terminal_executable(program: &str, cwd: &Path) -> Result<PathBuf, String> {
    let supplied = PathBuf::from(program);
    if supplied.is_absolute() || supplied.components().count() > 1 {
        let candidate = if supplied.is_absolute() {
            supplied
        } else {
            cwd.join(supplied)
        };
        return candidate.canonicalize().map_err(|error| {
            format!(
                "Failed to resolve terminal executable `{}`: {error}",
                candidate.display()
            )
        });
    }
    crate::diagnostics::resolve::resolve_all(program)
        .into_iter()
        .next()
        .ok_or_else(|| format!("Terminal executable `{program}` was not found on PATH"))?
        .canonicalize()
        .map_err(|error| format!("Failed to canonicalize terminal executable `{program}`: {error}"))
}

#[cfg(windows)]
fn managed_terminal_cwd(requested: &Path) -> Result<PathBuf, String> {
    if let Some(cwd) = existing_directory(requested) {
        return cwd
            .canonicalize()
            .map_err(|error| format!("Failed to resolve terminal cwd: {error}"));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
        if profile.is_dir() {
            return profile
                .canonicalize()
                .map_err(|error| format!("Failed to resolve terminal profile cwd: {error}"));
        }
    }
    std::env::current_dir().map_err(|error| format!("Failed to resolve terminal cwd: {error}"))
}

#[cfg(windows)]
fn spawn_suspended_managed_terminal(
    slave: &dyn SlavePty,
    session_id: &str,
    cwd: &Path,
    program: &str,
    args: &[String],
    env: HashMap<String, String>,
    mut authority: TerminalLaunchAuthority,
    io: Arc<ManagedTerminalIo>,
) -> Result<(Box<dyn Child + Send + Sync>, Arc<ManagedTerminalTeardown>), String> {
    // Reject caller-controlled strings and collections before path resolution
    // or duplicate native-string allocations. Reserve the eight fixed
    // terminal defaults, then validate their exact augmented values below.
    authority.ports = crate::process::teardown::validate_terminal_teardown_inputs(
        session_id,
        authority.action_epoch,
        &authority.ports,
    )?;
    validate_terminal_launch_source_bounds(
        OsStr::new(program),
        cwd.as_os_str(),
        program,
        args,
        &env,
        8,
    )
    .map_err(|error| format!("Invalid managed terminal launch: {error}"))?;
    let cwd = managed_terminal_cwd(cwd)?;
    let executable = resolve_terminal_executable(program, &cwd)?;
    let environment = with_terminal_env_defaults(env);
    validate_terminal_launch_source_bounds(
        executable.as_os_str(),
        cwd.as_os_str(),
        program,
        args,
        &environment,
        0,
    )
    .map_err(|error| format!("Invalid managed terminal launch: {error}"))?;
    let environment: BTreeMap<OsString, OsString> = environment
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect();
    let pending = prepare_suspended_pty(
        slave,
        LaunchIntent {
            resource_id: authority.resource_id,
            generation: authority.runtime_generation,
            owner: authority.owner,
            kind: ResourceKind::Terminal,
            executable,
            args: args.iter().map(OsString::from).collect(),
            cwd,
            environment,
            display_label: program.to_string(),
        },
    )
    .map_err(|error| format!("Failed to prepare managed terminal: {error}"))?;
    let (teardown, child) = ManagedTerminalTeardown::from_pending_launch(
        pending,
        authority.operation_id,
        authority.action_epoch,
        authority.completion_store,
        session_id.to_string(),
        authority.ports,
        Arc::clone(&io),
    )?;
    Ok((child.into_child(), teardown))
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

#[cfg(windows)]
fn cleanup_failed_spawn(teardown: &Arc<ManagedTerminalTeardown>) {
    // The suspended launcher does not return until Job registration and resume
    // are one committed handoff. Every later setup failure therefore has exact
    // coordinator authority; a raw PTY ChildKiller is never minted on Windows.
    match teardown.close() {
        Ok(report) if report.outcome() == crate::process::teardown::TeardownOutcome::Closed => {
            if !teardown.actors_joined() {
                eprintln!("managed terminal setup cleanup returned before all actors joined");
                std::process::abort();
            }
        }
        Ok(report) => {
            eprintln!(
                "managed terminal setup cleanup did not close exactly: {:?}",
                report.errors()
            );
            std::process::abort();
        }
        Err(error) => {
            eprintln!("managed terminal setup cleanup failed: {error}");
            std::process::abort();
        }
    }
}

#[cfg(not(windows))]
fn cleanup_failed_spawn(cleanup_killer: &mut Box<dyn ChildKiller + Send + Sync>) {
    let _ = cleanup_killer.kill();
}

#[cfg(not(windows))]
fn detach_pty_and_join_actor_slots(
    input_admission: &AtomicBool,
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    master: &Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    actors: &Arc<Mutex<TerminalActorHandles>>,
) -> Result<(), String> {
    let cancellation_deadline = std::time::Instant::now()
        .checked_add(terminal_actor_cancellation_timeout())
        .ok_or_else(|| "terminal actor cancellation deadline overflow".to_string())?;

    // Acquire every ownership slot before mutating any PTY handle. If a
    // caller is still using one of these slots, fail closed without detaching
    // a different live reader/writer/waiter.
    let mut writer =
        lock_terminal_actor_resource_until(writer, cancellation_deadline, "terminal PTY writer")?;
    input_admission.store(false, Ordering::Release);
    let mut master =
        lock_terminal_actor_resource_until(master, cancellation_deadline, "terminal PTY master")?;
    let mut actors = lock_terminal_actor_resource_until(
        actors,
        cancellation_deadline,
        "terminal actor handles",
    )?;
    let current = thread::current().id();
    for handle in [&actors.reader, &actors.waiter].into_iter().flatten() {
        if handle.thread().id() == current {
            return Err("terminal actor attempted to synchronously join itself".to_string());
        }
    }

    // Dropping the host PTY handles is the cancellation boundary for the
    // blocking reader. The process wait actor is terminal after the child is
    // killed or exits. Keep the JoinHandles in `actors` until both are known
    // to have stopped; a timeout must never silently detach a live actor.
    let old_writer = std::mem::replace(&mut *writer, Box::new(std::io::sink()));
    master.take();
    drop(old_writer);
    drop(master);
    drop(writer);

    while [&actors.reader, &actors.waiter]
        .into_iter()
        .flatten()
        .any(|handle| !handle.is_finished())
        && std::time::Instant::now() < cancellation_deadline
    {
        thread::yield_now();
    }
    if [&actors.reader, &actors.waiter]
        .into_iter()
        .flatten()
        .any(|handle| !handle.is_finished())
    {
        return Err("terminal actor did not acknowledge bounded PTY cancellation".to_string());
    }

    let mut join_error = None;
    if let Some(reader) = actors.reader.take() {
        if reader.join().is_err() {
            join_error = Some("terminal reader actor panicked".to_string());
        }
    }
    if let Some(waiter) = actors.waiter.take() {
        if waiter.join().is_err() && join_error.is_none() {
            join_error = Some("terminal wait actor panicked".to_string());
        }
    }
    if let Some(error) = join_error {
        return Err(error);
    }
    Ok(())
}

#[cfg(not(windows))]
fn lock_terminal_actor_resource_until<'a, T>(
    resource: &'a Mutex<T>,
    deadline: std::time::Instant,
    label: &str,
) -> Result<std::sync::MutexGuard<'a, T>, String> {
    loop {
        match resource.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    return Err(format!(
                        "{label} remained contended during bounded teardown"
                    ));
                }
                thread::yield_now();
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(format!("{label} poisoned"));
            }
        }
    }
}

#[cfg(not(windows))]
fn terminal_actor_cancellation_timeout() -> Duration {
    #[cfg(test)]
    {
        return Duration::from_millis(100);
    }
    #[cfg(not(test))]
    {
        Duration::from_secs(5)
    }
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
    state_notifier: Option<SessionStateNotifier>,
    output_notifier: Option<SessionOutputNotifier>,
    service_output_sink: Arc<Mutex<Option<TerminalOutputSink>>>,
    service_lifecycle_sink: Arc<Mutex<Option<TerminalLifecycleSink>>>,
    replay_buffer: Arc<Mutex<Vec<u8>>>,
    start_gate: Arc<TerminalActorStartGate>,
    #[cfg(windows)] teardown: Arc<Mutex<Option<Arc<ManagedTerminalTeardown>>>>,
) -> Result<thread::JoinHandle<()>, String> {
    thread::Builder::new()
        .name(format!("terminal-reader-{session_id}"))
        .spawn(move || {
            start_gate.wait();
            let mut parser = Processor::<StdSyncHandler>::new();
            let mut shell_sequences = ShellSequenceParser::default();
            let mut buffer = [0_u8; 4096];

            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        if debug_enabled {
                            eprintln!("[terminal:{session_id}] PTY reader reached EOF");
                        }
                        notify_service_lifecycle(
                            &service_lifecycle_sink,
                            TerminalLifecycleEvent::ReaderEof,
                        );
                        break;
                    }
                    Ok(bytes_read) => {
                        let mode = apply_terminal_output_chunk(
                            &session_id,
                            &buffer[..bytes_read],
                            &term,
                            &mut parser,
                            &mut shell_sequences,
                            &runtime_state,
                        );
                        append_replay_bytes(&replay_buffer, &buffer[..bytes_read]);

                        if let Some(writer) = log_writer.as_mut() {
                            writer.write_chunk(&buffer[..bytes_read]);
                        }

                        if let Some(notifier) = output_notifier.as_ref() {
                            notifier(buffer[..bytes_read].to_vec(), mode);
                        }
                        if let Ok(sink) = service_output_sink.lock() {
                            if let Some(notifier) = sink.as_ref() {
                                notifier(buffer[..bytes_read].to_vec(), mode);
                            }
                        }
                        if let Some(notifier) = state_notifier.as_ref() {
                            notifier();
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
                        notify_service_lifecycle(
                            &service_lifecycle_sink,
                            TerminalLifecycleEvent::ReaderFailed {
                                summary: format!("PTY read failed: {error}"),
                            },
                        );
                        if let Some(notifier) = state_notifier.as_ref() {
                            notifier();
                        }
                        #[cfg(windows)]
                        request_managed_terminal_teardown(&teardown);
                        break;
                    }
                }
            }

            if let Some(writer) = log_writer.as_mut() {
                writer.flush_remaining();
            }
        })
        .map_err(|error| format!("Failed to spawn terminal reader actor: {error}"))
}

fn notify_service_lifecycle(
    sink: &Arc<Mutex<Option<TerminalLifecycleSink>>>,
    event: TerminalLifecycleEvent,
) {
    if let Ok(guard) = sink.lock() {
        if let Some(notifier) = guard.as_ref() {
            notifier(event);
        }
    }
}

fn spawn_wait_thread(
    session_id: String,
    mut child: Box<dyn Child + Send + Sync>,
    #[cfg_attr(windows, allow(unused_variables))] pid: Option<u32>,
    #[cfg(windows)] teardown: Arc<Mutex<Option<Arc<ManagedTerminalTeardown>>>>,
    #[cfg(not(windows))] process_job: Arc<Mutex<Option<platform_service::ManagedProcessJob>>>,
    runtime_state: Arc<RwLock<RuntimeState>>,
    debug_enabled: bool,
    state_notifier: Option<SessionStateNotifier>,
    service_lifecycle_sink: Arc<Mutex<Option<TerminalLifecycleSink>>>,
    start_gate: Arc<TerminalActorStartGate>,
) -> Result<thread::JoinHandle<()>, String> {
    #[cfg(all(test, windows))]
    if FAIL_NEXT_WAIT_ACTOR_SPAWN.swap(false, Ordering::SeqCst) {
        return Err("injected wait actor spawn failure".to_string());
    }
    thread::Builder::new()
        .name(format!("terminal-wait-{session_id}"))
        .spawn(move || {
            start_gate.wait();
            match child.wait() {
                Ok(status) => {
                    if debug_enabled {
                        eprintln!("[terminal:{session_id}] child exit -> {status}");
                    }
                    #[cfg(windows)]
                    request_managed_terminal_teardown(&teardown);
                    #[cfg(not(windows))]
                    drop_managed_process_job(&process_job);
                    #[cfg(not(windows))]
                    thread::sleep(Duration::from_millis(50));
                    #[cfg(not(windows))]
                    let surviving_descendants = pid
                        .map(platform_service::collect_descendant_process_identities)
                        .unwrap_or_default();
                    let summary = if let Some(signal) = status.signal() {
                        format!("Shell terminated by {signal}")
                    } else {
                        format!("Shell exited with code {}", status.exit_code())
                    };
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
                                    summary: summary.clone(),
                                },
                                SessionStatus::Exited,
                            );
                        }
                    }
                    notify_service_lifecycle(
                        &service_lifecycle_sink,
                        TerminalLifecycleEvent::ChildExited {
                            summary,
                            code: Some(status.exit_code()),
                        },
                    );
                    if let Some(notifier) = state_notifier.as_ref() {
                        notifier();
                    }
                    #[cfg(not(windows))]
                    if let Some(pid) = pid {
                        let _ =
                            pid_file::release_session_root(&session_id, pid, surviving_descendants);
                    }
                }
                Err(error) => {
                    if debug_enabled {
                        eprintln!("[terminal:{session_id}] wait error: {error}");
                    }
                    #[cfg(windows)]
                    request_managed_terminal_teardown(&teardown);
                    #[cfg(not(windows))]
                    drop_managed_process_job(&process_job);
                    #[cfg(not(windows))]
                    thread::sleep(Duration::from_millis(50));
                    #[cfg(not(windows))]
                    let surviving_descendants = pid
                        .map(platform_service::collect_descendant_process_identities)
                        .unwrap_or_default();
                    let summary = format!("Failed while waiting for shell exit: {error}");
                    if let Ok(mut runtime) = runtime_state.write() {
                        if let Some(session) = runtime.sessions.get_mut(&session_id) {
                            session.note_exit(
                                SessionExitState {
                                    code: None,
                                    signal: None,
                                    closed_by_user: false,
                                    summary: summary.clone(),
                                },
                                SessionStatus::Failed,
                            );
                        }
                    }
                    notify_service_lifecycle(
                        &service_lifecycle_sink,
                        TerminalLifecycleEvent::ChildExited {
                            summary,
                            code: None,
                        },
                    );
                    if let Some(notifier) = state_notifier.as_ref() {
                        notifier();
                    }
                    #[cfg(not(windows))]
                    if let Some(pid) = pid {
                        let _ =
                            pid_file::release_session_root(&session_id, pid, surviving_descendants);
                    }
                }
            }
        })
        .map_err(|error| format!("Failed to spawn terminal wait actor: {error}"))
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
    state_notifier: Option<SessionStateNotifier>,
    output_notifier: Option<SessionOutputNotifier>,
    authority: TerminalLaunchAuthority,
) -> Result<TerminalSession, String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(pty_size(dimensions))
        .map_err(|error| error.to_string())?;
    let scrolling_history = scrolling_history.max(100);
    #[cfg(not(windows))]
    let service_attachment_fence = (authority.resource_id, authority.runtime_generation);

    #[cfg(windows)]
    let writer = Arc::new(Mutex::new(
        Box::new(std::io::sink()) as Box<dyn Write + Send>
    ));
    let input_admission = Arc::new(AtomicBool::new(false));
    #[cfg(windows)]
    let master = Arc::new(Mutex::new(None));
    let actors = Arc::new(Mutex::new(TerminalActorHandles::default()));
    #[cfg(windows)]
    let io = ManagedTerminalIo::new(
        Arc::clone(&writer),
        Arc::clone(&master),
        Arc::clone(&actors),
        Arc::clone(&input_admission),
    );
    #[cfg(windows)]
    let (child, teardown) = spawn_suspended_managed_terminal(
        &*pair.slave,
        session_id,
        &cwd,
        &program,
        &args,
        env,
        authority,
        Arc::clone(&io),
    )?;
    #[cfg(not(windows))]
    let child = {
        let mut command = CommandBuilder::new(program.clone());
        if let Some(valid_cwd) = existing_directory(&cwd) {
            command.cwd(valid_cwd);
        }
        if !args.is_empty() {
            command.args(args.clone());
        }
        apply_terminal_env_defaults(&mut command, env);
        pair.slave
            .spawn_command(command)
            .map_err(|error| format!("Failed to spawn command: {error}"))?
    };

    let pid = child.process_id();
    #[cfg(not(windows))]
    let mut cleanup_killer = child.clone_killer();
    #[cfg(not(windows))]
    let process_job = attach_managed_process_job(pid);
    let acquired_writer = match pair.master.take_writer() {
        Ok(writer) => writer,
        Err(error) => {
            cleanup_failed_spawn(
                #[cfg(not(windows))]
                &mut cleanup_killer,
                #[cfg(windows)]
                &teardown,
            );
            return Err(format!("Failed to acquire PTY writer: {error}"));
        }
    };
    #[cfg(windows)]
    {
        let mut writer_slot = match writer.lock() {
            Ok(writer_slot) => writer_slot,
            Err(_) => {
                cleanup_failed_spawn(&teardown);
                return Err("terminal PTY writer poisoned".to_string());
            }
        };
        *writer_slot = acquired_writer;
    }
    let reader = match pair.master.try_clone_reader() {
        Ok(reader) => reader,
        Err(error) => {
            cleanup_failed_spawn(
                #[cfg(not(windows))]
                &mut cleanup_killer,
                #[cfg(windows)]
                &teardown,
            );
            return Err(format!("Failed to clone PTY reader: {error}"));
        }
    };
    let log_writer = open_log_writer(log_file_path);

    #[cfg(not(windows))]
    let writer = Arc::new(Mutex::new(acquired_writer));
    #[cfg(windows)]
    {
        let mut master_slot = match master.lock() {
            Ok(master_slot) => master_slot,
            Err(_) => {
                cleanup_failed_spawn(&teardown);
                return Err("terminal PTY master poisoned".to_string());
            }
        };
        *master_slot = Some(pair.master);
    }
    #[cfg(not(windows))]
    let master = Arc::new(Mutex::new(Some(pair.master)));
    #[cfg(not(windows))]
    let killer = Arc::new(Mutex::new(child.clone_killer()));
    #[cfg(not(windows))]
    let process_job = Arc::new(Mutex::new(process_job));
    let dimensions_state = Arc::new(Mutex::new(dimensions));
    let replay_buffer = Arc::new(Mutex::new(Vec::new()));
    let event_proxy = SessionEventProxy {
        session_id: session_id.to_string(),
        writer: writer.clone(),
        input_admission: Arc::clone(&input_admission),
        runtime_state: runtime_state.clone(),
        dimensions: dimensions_state.clone(),
        debug_enabled,
        state_notifier: state_notifier.clone(),
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
            cleanup_failed_spawn(
                #[cfg(not(windows))]
                &mut cleanup_killer,
                #[cfg(windows)]
                &teardown,
            );
            return Err(error);
        }
    }

    #[cfg(windows)]
    let teardown_handle = Arc::new(Mutex::new(Some(teardown.clone())));
    let mut actor_slots = match actors.lock() {
        Ok(actor_slots) => actor_slots,
        Err(_) => {
            cleanup_failed_spawn(
                #[cfg(not(windows))]
                &mut cleanup_killer,
                #[cfg(windows)]
                &teardown,
            );
            return Err("terminal actor handles poisoned".to_string());
        }
    };
    let start_gate = Arc::new(TerminalActorStartGate::default());
    let service_output_sink = Arc::new(Mutex::new(None));
    let service_lifecycle_sink = Arc::new(Mutex::new(None));
    let reader_actor = match spawn_reader_thread(
        session_id.to_string(),
        reader,
        term.clone(),
        log_writer,
        runtime_state.clone(),
        debug_enabled,
        state_notifier.clone(),
        output_notifier.clone(),
        Arc::clone(&service_output_sink),
        Arc::clone(&service_lifecycle_sink),
        replay_buffer.clone(),
        Arc::clone(&start_gate),
        #[cfg(windows)]
        teardown_handle.clone(),
    ) {
        Ok(actor) => actor,
        Err(error) => {
            drop(actor_slots);
            cleanup_failed_spawn(
                #[cfg(not(windows))]
                &mut cleanup_killer,
                #[cfg(windows)]
                &teardown,
            );
            return Err(error);
        }
    };
    actor_slots.reader = Some(reader_actor);

    let wait_actor = match spawn_wait_thread(
        session_id.to_string(),
        child,
        pid,
        #[cfg(windows)]
        teardown_handle,
        #[cfg(not(windows))]
        process_job.clone(),
        runtime_state.clone(),
        debug_enabled,
        state_notifier,
        Arc::clone(&service_lifecycle_sink),
        Arc::clone(&start_gate),
    ) {
        Ok(actor) => actor,
        Err(error) => {
            drop(actor_slots);
            start_gate.release();
            cleanup_failed_spawn(
                #[cfg(not(windows))]
                &mut cleanup_killer,
                #[cfg(windows)]
                &teardown,
            );
            #[cfg(not(windows))]
            if detach_pty_and_join_actor_slots(&input_admission, &writer, &master, &actors).is_err()
            {
                std::process::abort();
            }
            return Err(error);
        }
    };
    actor_slots.waiter = Some(wait_actor);
    drop(actor_slots);
    start_gate.release();
    #[cfg(windows)]
    if let Err(error) = open_managed_terminal_input(&io) {
        cleanup_failed_spawn(&teardown);
        return Err(error);
    }
    #[cfg(not(windows))]
    input_admission.store(true, Ordering::Release);

    event_proxy.debug_log(format!("spawned {}", program));

    Ok(TerminalSession {
        session_id: session_id.to_string(),
        term,
        writer,
        input_admission,
        master,
        actors,
        #[cfg(not(windows))]
        killer,
        #[cfg(windows)]
        teardown: Arc::new(Mutex::new(Some(teardown))),
        #[cfg(windows)]
        lifecycle: Mutex::new(()),
        #[cfg(windows)]
        retired: AtomicBool::new(false),
        #[cfg(all(test, windows))]
        managed_resource_publication_barrier: Mutex::new(None),
        #[cfg(not(windows))]
        process_job,
        runtime_state,
        dimensions: dimensions_state,
        event_proxy,
        backend,
        scrolling_history,
        replay_buffer,
        output_notifier,
        service_output_sink,
        service_lifecycle_sink,
        #[cfg(not(windows))]
        service_attachment_pin: Mutex::new(Some(service_attachment_fence)),
    })
}

#[cfg(not(windows))]
fn drop_managed_process_job(process_job: &Arc<Mutex<Option<platform_service::ManagedProcessJob>>>) {
    if let Ok(mut process_job) = process_job.lock() {
        process_job.take();
    }
}

impl TerminalSession {
    #[cfg(not(windows))]
    fn validate_service_restart_fence(
        &self,
        authority: &TerminalLaunchAuthority,
    ) -> Result<(), String> {
        let pin = self
            .service_attachment_pin
            .lock()
            .map_err(|_| "terminal attachment pin poisoned".to_string())?;
        if let Some((resource_id, generation)) = *pin {
            if resource_id == authority.resource_id && authority.runtime_generation <= generation {
                return Err(
                    "terminal restart authority is not newer than the attached generation"
                        .to_string(),
                );
            }
        }
        Ok(())
    }

    #[cfg(not(windows))]
    fn publish_service_attachment_fence(
        &self,
        authority: &TerminalLaunchAuthority,
    ) -> Result<(), String> {
        let mut pin = self
            .service_attachment_pin
            .lock()
            .map_err(|_| "terminal attachment pin poisoned".to_string())?;
        *pin = Some((authority.resource_id, authority.runtime_generation));
        Ok(())
    }

    /// Returns only the current exact teardown fence. This performs no Job
    /// enumeration and grants no raw process or termination capability.
    #[cfg(windows)]
    pub(crate) fn managed_process_fence(&self) -> Result<Option<ManagedProcessFence>, String> {
        let _lifecycle = lock_terminal_lifecycle(&self.lifecycle)?;
        let teardown = lock_terminal_teardown_slot(&self.teardown)?.clone();
        Ok(teardown.map(|teardown| teardown.managed_process_fence()))
    }

    /// Snapshots the exact managed root fence and current Job membership from
    /// the teardown-owned registry. This deliberately exposes neither the Job
    /// handle nor any raw termination operation.
    #[cfg(all(test, windows))]
    pub(crate) fn managed_process_snapshot(&self) -> Option<(ManagedProcessFence, Vec<u32>)> {
        let teardown = lock_terminal_teardown_slot(&self.teardown).ok()?.clone()?;
        teardown.managed_process_snapshot().ok()
    }

    /// Returns the exact generation/identity fence and exact current Job
    /// observations under one caller-supplied absolute deadline. This is a
    /// read-only accounting seam; the Job handle and close authority remain
    /// sealed inside teardown.
    #[cfg(windows)]
    pub(crate) fn managed_process_observations_until(
        &self,
        absolute_deadline: std::time::Instant,
        max_members: usize,
    ) -> Result<Option<ManagedProcessObservationQuery>, String> {
        if std::time::Instant::now() >= absolute_deadline {
            return Err("terminal managed-process observation exceeded deadline".to_string());
        }
        let teardown =
            lock_terminal_teardown_slot_until(&self.teardown, absolute_deadline)?.clone();
        if std::time::Instant::now() >= absolute_deadline {
            return Err("terminal managed-process observation exceeded deadline".to_string());
        }
        let Some(teardown) = teardown else {
            return Ok(None);
        };
        let fence = teardown.managed_process_fence();
        let members = teardown
            .managed_process_observations_until(absolute_deadline, max_members)
            .and_then(|(observed_fence, members)| {
                if observed_fence == fence {
                    Ok(members)
                } else {
                    Err("terminal managed-process observation generation changed".to_string())
                }
            });
        Ok(Some(ManagedProcessObservationQuery {
            capture: ManagedProcessObservationCapture { teardown, fence },
            members,
        }))
    }

    /// Publishes one accounting snapshot only if the exact teardown Arc,
    /// registry fence, runtime generation, and root PID captured by the Job
    /// query are still current. Lock order intentionally matches restart and
    /// asynchronous teardown: lifecycle, publication guard, teardown state,
    /// then runtime projection. Release takes only the publication write guard
    /// and teardown state, so it cannot invalidate authority during commit.
    #[cfg(windows)]
    pub(crate) fn publish_managed_resource_sample_if_current(
        &self,
        capture: &ManagedProcessObservationCapture,
        snapshot: ResourceSnapshot,
        awaiting_external_editor: bool,
        absolute_deadline: std::time::Instant,
    ) -> Result<ManagedResourceSamplePublication, String> {
        let _lifecycle = lock_terminal_lifecycle_until(&self.lifecycle, absolute_deadline)?;
        let current_teardown =
            lock_terminal_teardown_slot_until(&self.teardown, absolute_deadline)?.clone();
        let exact_teardown_is_current = current_teardown.as_ref().is_some_and(|current| {
            Arc::ptr_eq(current, &capture.teardown)
                && current.matches_fence(&capture.fence)
                && capture.teardown.matches_fence(&capture.fence)
        });
        let _publication_guard = exact_teardown_is_current
            .then(|| {
                capture
                    .teardown
                    .lock_resource_publication_until(absolute_deadline)
            })
            .transpose()?;
        let exact_registry_is_current = exact_teardown_is_current
            && capture
                .teardown
                .exact_registry_entry_is_current_until(absolute_deadline)?;
        #[cfg(all(test, windows))]
        if exact_registry_is_current {
            self.pause_managed_resource_publication_after_validation_for_test()?;
        }

        let mut runtime =
            lock_terminal_runtime_write_until(&self.runtime_state, absolute_deadline)?;
        let Some(session) = runtime.sessions.get_mut(&self.session_id) else {
            return Ok(ManagedResourceSamplePublication::StaleGeneration {
                dirty_changed: false,
            });
        };
        if std::time::Instant::now() >= absolute_deadline {
            return Err("terminal resource publication exceeded deadline".to_string());
        }
        let dirty_before = session.dirty_generation;
        let exact_runtime_is_current =
            session.status.is_live() && session.pid == Some(capture.fence.root().id().pid());
        if !exact_teardown_is_current || !exact_registry_is_current || !exact_runtime_is_current {
            // Never disturb a replacement generation. If the row still holds
            // the rejected G1 action fence, remove that local authority and
            // explicitly type the retained values as stale/unknown.
            if session.resources.managed_process_fence.as_ref() == Some(&capture.fence) {
                session.resources.managed_process_fence = None;
                session.resources.metrics_unavailable = true;
                session.resources.metrics_status =
                    crate::domain::snapshot::ProcessMetricStatus::Unknown;
                session.resources.metrics_stale = true;
                session.resources.metrics_error = Some("sampling_generation_stale".to_string());
                session.resources.process_count_value_state =
                    last_known_resource_value_state(session.resources.process_count_value_state);
                session.resources.cpu_value_state =
                    last_known_resource_value_state(session.resources.cpu_value_state);
                session.resources.memory_value_state =
                    last_known_resource_value_state(session.resources.memory_value_state);
                session.resources.metric_values =
                    last_known_resource_value_state(session.resources.metric_values);
                session.mark_dirty();
            }
            return Ok(ManagedResourceSamplePublication::StaleGeneration {
                dirty_changed: session.dirty_generation != dirty_before,
            });
        }

        if std::time::Instant::now() >= absolute_deadline {
            return Err("terminal resource publication exceeded deadline".to_string());
        }
        let cleared_unreaped = session.reap_incomplete && snapshot.process_ids.is_empty();
        session.note_resource_sample(snapshot);
        session.note_external_editor_wait(awaiting_external_editor);
        Ok(ManagedResourceSamplePublication::Published {
            dirty_changed: session.dirty_generation != dirty_before,
            cleared_unreaped,
        })
    }

    #[cfg(all(test, windows))]
    fn install_managed_resource_publication_barrier_for_test(
        &self,
        validated: std::sync::mpsc::SyncSender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) {
        *self
            .managed_resource_publication_barrier
            .lock()
            .expect("managed resource publication test barrier") =
            Some(ManagedResourcePublicationTestBarrier { validated, resume });
    }

    #[cfg(all(test, windows))]
    fn pause_managed_resource_publication_after_validation_for_test(&self) -> Result<(), String> {
        let barrier = self
            .managed_resource_publication_barrier
            .lock()
            .map_err(|_| "managed resource publication test barrier poisoned".to_string())?
            .take();
        let Some(barrier) = barrier else {
            return Ok(());
        };
        barrier
            .validated
            .send(())
            .map_err(|_| "managed resource publication test signal was dropped".to_string())?;
        barrier
            .resume
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "managed resource publication test release timed out".to_string())
    }
}

#[cfg(windows)]
fn last_known_resource_value_state(
    state: crate::state::ResourceMetricValueState,
) -> crate::state::ResourceMetricValueState {
    match state {
        crate::state::ResourceMetricValueState::Observed
        | crate::state::ResourceMetricValueState::Partial
        | crate::state::ResourceMetricValueState::LastKnown => {
            crate::state::ResourceMetricValueState::LastKnown
        }
        crate::state::ResourceMetricValueState::Unavailable => {
            crate::state::ResourceMetricValueState::Unavailable
        }
    }
}

#[cfg(windows)]
fn request_managed_terminal_teardown(teardown: &Arc<Mutex<Option<Arc<ManagedTerminalTeardown>>>>) {
    // Reader/wait actors must never block each other or host shutdown on this
    // projection slot. A missed request is harmless: the retained session
    // owner still performs the same exact close on reconciliation/drop.
    let context = teardown.try_lock().ok().and_then(|slot| slot.clone());
    if let Some(context) = context {
        let _ = context.request_close();
    }
}

#[cfg(windows)]
fn lock_terminal_lifecycle(lifecycle: &Mutex<()>) -> Result<MutexGuard<'_, ()>, String> {
    let deadline = std::time::Instant::now()
        .checked_add(Duration::from_millis(100))
        .ok_or_else(|| "terminal lifecycle deadline overflow".to_string())?;
    lock_terminal_lifecycle_until(lifecycle, deadline)
}

#[cfg(windows)]
fn lock_terminal_lifecycle_until(
    lifecycle: &Mutex<()>,
    deadline: std::time::Instant,
) -> Result<MutexGuard<'_, ()>, String> {
    loop {
        if std::time::Instant::now() >= deadline {
            return Err("terminal lifecycle remained contended".to_string());
        }
        match lifecycle.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::WouldBlock) => thread::yield_now(),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("terminal lifecycle poisoned".to_string())
            }
        }
    }
}

#[cfg(windows)]
fn lock_terminal_runtime_write_until<'a>(
    runtime_state: &'a Arc<RwLock<RuntimeState>>,
    deadline: std::time::Instant,
) -> Result<RwLockWriteGuard<'a, RuntimeState>, String> {
    loop {
        if std::time::Instant::now() >= deadline {
            return Err("terminal runtime projection remained contended".to_string());
        }
        match runtime_state.try_write() {
            Ok(runtime) => {
                if std::time::Instant::now() >= deadline {
                    return Err("terminal runtime projection remained contended".to_string());
                }
                return Ok(runtime);
            }
            Err(std::sync::TryLockError::WouldBlock) => thread::yield_now(),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("terminal runtime projection poisoned".to_string())
            }
        }
    }
}

#[cfg(windows)]
fn lock_terminal_teardown_slot<'a>(
    teardown: &'a Arc<Mutex<Option<Arc<ManagedTerminalTeardown>>>>,
) -> Result<MutexGuard<'a, Option<Arc<ManagedTerminalTeardown>>>, String> {
    let deadline = std::time::Instant::now()
        .checked_add(Duration::from_millis(100))
        .ok_or_else(|| "terminal teardown slot deadline overflow".to_string())?;
    lock_terminal_teardown_slot_until(teardown, deadline)
}

#[cfg(windows)]
fn lock_terminal_teardown_slot_until<'a>(
    teardown: &'a Arc<Mutex<Option<Arc<ManagedTerminalTeardown>>>>,
    deadline: std::time::Instant,
) -> Result<MutexGuard<'a, Option<Arc<ManagedTerminalTeardown>>>, String> {
    loop {
        if std::time::Instant::now() >= deadline {
            return Err("terminal teardown slot remained contended".to_string());
        }
        match teardown.try_lock() {
            Ok(slot) => {
                if std::time::Instant::now() >= deadline {
                    return Err("terminal teardown slot remained contended".to_string());
                }
                return Ok(slot);
            }
            Err(std::sync::TryLockError::WouldBlock) => thread::yield_now(),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("terminal teardown slot poisoned".to_string())
            }
        }
    }
}

#[cfg(windows)]
fn open_managed_terminal_input(io: &ManagedTerminalIo) -> Result<(), String> {
    #[cfg(test)]
    if FAIL_NEXT_INPUT_ADMISSION_OPEN.swap(false, Ordering::SeqCst) {
        return Err("injected terminal input-admission open failure".to_string());
    }
    io.open_input_after_start()
}

#[cfg(not(windows))]
fn attach_managed_process_job(pid: Option<u32>) -> Option<platform_service::ManagedProcessJob> {
    pid.and_then(
        |pid| match platform_service::attach_process_to_managed_job(pid) {
            Ok(job) => job,
            Err(error) => {
                eprintln!("[terminal] managed job attach failed for pid {pid}: {error}");
                None
            }
        },
    )
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

fn apply_terminal_output_chunk(
    session_id: &str,
    bytes: &[u8],
    term: &Arc<Mutex<Term<SessionEventProxy>>>,
    parser: &mut Processor<StdSyncHandler>,
    shell_sequences: &mut ShellSequenceParser,
    runtime_state: &Arc<RwLock<RuntimeState>>,
) -> TerminalModeSnapshot {
    let parsed_sequences = shell_sequences.push_chunk(bytes);
    let (cursor_buffer_line, mode) = {
        let mut term = match term.lock() {
            Ok(term) => term,
            Err(error) => error.into_inner(),
        };
        parser.advance(&mut *term, bytes);
        (
            terminal_cursor_buffer_line(&term),
            mode_snapshot(*term.mode()),
        )
    };

    if let Ok(mut runtime) = runtime_state.write() {
        if let Some(session) = runtime.sessions.get_mut(session_id) {
            session.record_pty_bytes(bytes.len());
            session.note_output_activity();
            apply_shell_sequences(session, &parsed_sequences, cursor_buffer_line);
        }
    }
    mode
}

fn append_replay_bytes(buffer: &Arc<Mutex<Vec<u8>>>, bytes: &[u8]) {
    let Ok(mut replay) = buffer.lock() else {
        return;
    };
    replay.extend_from_slice(bytes);
    if replay.len() > MAX_REMOTE_REPLAY_BYTES {
        let overflow = replay.len().saturating_sub(MAX_REMOTE_REPLAY_BYTES);
        replay.drain(0..overflow);
    }
}

fn apply_display_offset_to_term(term: &mut Term<SessionEventProxy>, target: usize) {
    let history_size = term
        .grid()
        .total_lines()
        .saturating_sub(term.grid().screen_lines());
    let target = target.min(history_size);
    let current = term.grid().display_offset();
    if target == current {
        return;
    }
    let delta = target as i32 - current as i32;
    term.scroll_display(Scroll::Delta(delta));
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

    #[cfg(windows)]
    #[test]
    fn managed_observation_slot_uses_the_callers_expired_deadline() {
        let teardown = Arc::new(Mutex::new(None));
        let error = match lock_terminal_teardown_slot_until(&teardown, std::time::Instant::now()) {
            Ok(_) => panic!("expired accounting deadline must fail before locking"),
            Err(error) => error,
        };
        assert!(error.contains("remained contended"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn production_terminal_spawn_uses_suspended_managed_launch() {
        let launches_before = crate::process::launcher::managed_launch_count_for_test();
        let runtime = Arc::new(RwLock::new(RuntimeState::default()));
        let journal = tempfile::tempdir().expect("terminal teardown journal");
        let resource_id = ResourceId::new();
        let completion_store =
            TeardownCompletionStore::durable(journal.path().join("teardown.sqlite3"))
                .expect("durable terminal teardown store");
        let authority = TerminalLaunchAuthority::new(
            ProcessOwner::Host,
            resource_id,
            1,
            OperationId::new(),
            1,
            Vec::new(),
            completion_store.clone(),
        )
        .expect("terminal launch authority");
        let session = spawn_with_command(
            "suspended-managed-terminal-test",
            std::env::current_dir().expect("test cwd"),
            SessionDimensions::default(),
            "cmd.exe".to_string(),
            vec![
                "/d".to_string(),
                "/s".to_string(),
                "/c".to_string(),
                "ping -n 30 127.0.0.1 >nul".to_string(),
            ],
            HashMap::new(),
            100,
            None,
            runtime,
            false,
            TerminalBackend::PortablePtyFeedingAlacritty,
            false,
            None,
            None,
            authority,
        )
        .expect("spawn production terminal");

        assert_eq!(
            crate::process::launcher::managed_launch_count_for_test(),
            launches_before + 1,
            "the production terminal path must register its suspended root before resume"
        );
        let (fence, active_process_ids) = session
            .managed_process_snapshot()
            .expect("exact managed process snapshot");
        assert!(
            active_process_ids.contains(&fence.root().id().pid()),
            "the root named by the exact PID/creation/executable/generation fence must be in the authoritative Job"
        );
        let observation_deadline = std::time::Instant::now()
            .checked_add(Duration::from_secs(1))
            .expect("observation deadline");
        let observation = session
            .managed_process_observations_until(observation_deadline, 32)
            .expect("bounded observation query")
            .expect("managed observation authority");
        let (observation_capture, observations) = observation.into_parts();
        let observations = observations.expect("managed Job observations");
        assert_eq!(observation_capture.fence(), &fence);
        assert!(observations.iter().any(|observation| matches!(
            observation,
            crate::process::job::JobMemberObservation::Accessible { identity }
                if identity == fence.root()
        )));
        let restart_authority = TerminalLaunchAuthority::new(
            ProcessOwner::Host,
            resource_id,
            2,
            OperationId::new(),
            2,
            Vec::new(),
            completion_store,
        )
        .expect("terminal restart authority");
        session
            .restart_command(
                std::env::current_dir().expect("restart test cwd"),
                SessionDimensions::default(),
                "cmd.exe".to_string(),
                vec![
                    "/d".to_string(),
                    "/s".to_string(),
                    "/c".to_string(),
                    "ping -n 30 127.0.0.1 >nul".to_string(),
                ],
                HashMap::new(),
                None,
                false,
                restart_authority,
            )
            .expect("restart through suspended managed launch");
        assert_eq!(
            crate::process::launcher::managed_launch_count_for_test(),
            launches_before + 2,
            "restart must use the same suspended-in-Job production path"
        );
        let (restart_fence, restart_process_ids) = session
            .managed_process_snapshot()
            .expect("restarted exact managed process snapshot");
        assert_eq!(restart_fence.resource().runtime_generation, 2);
        assert!(restart_process_ids.contains(&restart_fence.root().id().pid()));
        session.close(false).expect("close managed terminal");
        assert_eq!(
            session.live_actor_count_for_test(),
            0,
            "terminal close must join its reader and wait actors before returning"
        );
    }

    #[cfg(windows)]
    #[test]
    fn stale_sampling_capture_cannot_publish_or_close_restarted_generation() {
        let runtime = Arc::new(RwLock::new(RuntimeState::default()));
        let journal = tempfile::tempdir().expect("terminal sampling-generation journal");
        let resource_id = ResourceId::new();
        let completion_store =
            TeardownCompletionStore::durable(journal.path().join("teardown.sqlite3"))
                .expect("durable terminal teardown store");
        let authority = TerminalLaunchAuthority::new(
            ProcessOwner::Host,
            resource_id,
            1,
            OperationId::new(),
            1,
            Vec::new(),
            completion_store.clone(),
        )
        .expect("terminal launch authority");
        let session = Arc::new(
            spawn_with_command(
                "sampling-generation-barrier-test",
                std::env::current_dir().expect("test cwd"),
                SessionDimensions::default(),
                "cmd.exe".to_string(),
                vec![
                    "/d".to_string(),
                    "/s".to_string(),
                    "/c".to_string(),
                    "ping -n 30 127.0.0.1 >nul".to_string(),
                ],
                HashMap::new(),
                100,
                None,
                Arc::clone(&runtime),
                false,
                TerminalBackend::PortablePtyFeedingAlacritty,
                false,
                None,
                None,
                authority,
            )
            .expect("spawn first managed generation"),
        );

        let (captured_tx, captured_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let sampling_session = Arc::clone(&session);
        let sampler = thread::spawn(move || {
            let query = sampling_session
                .managed_process_observations_until(
                    std::time::Instant::now() + Duration::from_secs(2),
                    32,
                )
                .expect("first-generation Job query")
                .expect("first-generation managed authority");
            let (capture, members) = query.into_parts();
            members.expect("first-generation Job members");
            let old_fence = capture.fence().clone();
            captured_tx
                .send(old_fence.clone())
                .expect("publish after-query barrier");
            release_rx.recv().expect("release stale sampler");

            let stale_snapshot = crate::state::ResourceSnapshot {
                process_count: 1,
                process_count_value_state: crate::state::ResourceMetricValueState::Observed,
                process_ids: vec![old_fence.root().id().pid()],
                managed_process_fence: Some(old_fence),
                metrics_status: crate::domain::snapshot::ProcessMetricStatus::Complete,
                metric_values: crate::state::ResourceMetricValueState::Observed,
                cpu_value_state: crate::state::ResourceMetricValueState::Observed,
                memory_value_state: crate::state::ResourceMetricValueState::Observed,
                ..crate::state::ResourceSnapshot::default()
            };
            sampling_session.publish_managed_resource_sample_if_current(
                &capture,
                stale_snapshot,
                false,
                std::time::Instant::now() + Duration::from_secs(2),
            )
        });

        let old_fence = captured_rx.recv().expect("first generation captured");
        let restart_authority = TerminalLaunchAuthority::new(
            ProcessOwner::Host,
            resource_id,
            2,
            OperationId::new(),
            2,
            Vec::new(),
            completion_store,
        )
        .expect("terminal restart authority");
        session
            .restart_command(
                std::env::current_dir().expect("restart test cwd"),
                SessionDimensions::default(),
                "cmd.exe".to_string(),
                vec![
                    "/d".to_string(),
                    "/s".to_string(),
                    "/c".to_string(),
                    "ping -n 30 127.0.0.1 >nul".to_string(),
                ],
                HashMap::new(),
                None,
                false,
                restart_authority,
            )
            .expect("install second managed generation");
        let restart_fence = session
            .managed_process_snapshot()
            .expect("second-generation managed snapshot")
            .0;
        assert_eq!(restart_fence.resource().runtime_generation, 2);
        runtime
            .write()
            .expect("runtime state")
            .sessions
            .get_mut("sampling-generation-barrier-test")
            .expect("second-generation runtime")
            .note_resource_sample(crate::state::ResourceSnapshot {
                process_count: 1,
                process_count_value_state: crate::state::ResourceMetricValueState::Observed,
                process_ids: vec![restart_fence.root().id().pid()],
                managed_process_fence: Some(restart_fence.clone()),
                metrics_status: crate::domain::snapshot::ProcessMetricStatus::Complete,
                metric_values: crate::state::ResourceMetricValueState::Observed,
                cpu_value_state: crate::state::ResourceMetricValueState::Observed,
                memory_value_state: crate::state::ResourceMetricValueState::Observed,
                ..crate::state::ResourceSnapshot::default()
            });

        release_tx
            .send(())
            .expect("release first-generation sample");
        assert_eq!(
            sampler.join().expect("join stale sampler"),
            Ok(ManagedResourceSamplePublication::StaleGeneration {
                dirty_changed: false
            })
        );

        let current = runtime
            .read()
            .expect("runtime state")
            .sessions
            .get("sampling-generation-barrier-test")
            .cloned()
            .expect("current runtime generation");
        assert_eq!(current.pid, Some(restart_fence.root().id().pid()));
        assert_eq!(current.resources.process_count, 1);
        assert_eq!(
            current.resources.process_count_value_state,
            crate::state::ResourceMetricValueState::Observed
        );
        assert!(!current
            .resources
            .process_ids
            .contains(&old_fence.root().id().pid()));
        assert_eq!(
            current.resources.managed_process_fence.as_ref(),
            Some(&restart_fence)
        );

        let stale_action_error = session
            .close_managed_process_exact(&old_fence, true)
            .expect_err("an old monitor action must not close the replacement generation");
        assert!(
            stale_action_error.contains("generation changed"),
            "unexpected stale action error: {stale_action_error}"
        );
        assert!(
            platform_service::is_pid_running(restart_fence.root().id().pid()),
            "the replacement generation must remain alive"
        );
        session.close(false).expect("close replacement generation");
    }

    #[cfg(windows)]
    #[test]
    fn asynchronous_registry_release_cannot_cross_a_validated_sample_commit() {
        let runtime = Arc::new(RwLock::new(RuntimeState::default()));
        let journal = tempfile::tempdir().expect("terminal release/publication journal");
        let authority = TerminalLaunchAuthority::new(
            ProcessOwner::Host,
            ResourceId::new(),
            1,
            OperationId::new(),
            1,
            Vec::new(),
            TeardownCompletionStore::durable(journal.path().join("teardown.sqlite3"))
                .expect("durable terminal teardown store"),
        )
        .expect("terminal launch authority");
        let session = Arc::new(
            spawn_with_command(
                "sampling-release-barrier-test",
                std::env::current_dir().expect("test cwd"),
                SessionDimensions::default(),
                "cmd.exe".to_string(),
                vec![
                    "/d".to_string(),
                    "/s".to_string(),
                    "/c".to_string(),
                    "ping -n 30 127.0.0.1 >nul".to_string(),
                ],
                HashMap::new(),
                100,
                None,
                Arc::clone(&runtime),
                false,
                TerminalBackend::PortablePtyFeedingAlacritty,
                false,
                None,
                None,
                authority,
            )
            .expect("spawn managed generation"),
        );
        let teardown = lock_terminal_teardown_slot(&session.teardown)
            .expect("teardown slot")
            .clone()
            .expect("managed teardown");

        let (release_attempted_tx, release_attempted_rx) = std::sync::mpsc::sync_channel(1);
        let (release_resume_tx, release_resume_rx) = std::sync::mpsc::sync_channel(0);
        let (released_tx, released_rx) = std::sync::mpsc::sync_channel(1);
        teardown.install_release_barrier_for_test(
            release_attempted_tx,
            release_resume_rx,
            released_tx,
        );

        let query = session
            .managed_process_observations_until(
                std::time::Instant::now() + Duration::from_secs(2),
                32,
            )
            .expect("exact Job query")
            .expect("managed observation authority");
        let (capture, members) = query.into_parts();
        members.expect("exact Job members");
        let fence = capture.fence().clone();
        let (validated_tx, validated_rx) = std::sync::mpsc::sync_channel(1);
        let (publication_resume_tx, publication_resume_rx) = std::sync::mpsc::sync_channel(0);
        session.install_managed_resource_publication_barrier_for_test(
            validated_tx,
            publication_resume_rx,
        );

        let sampling_session = Arc::clone(&session);
        let sample_fence = fence.clone();
        let sampler = thread::spawn(move || {
            sampling_session.publish_managed_resource_sample_if_current(
                &capture,
                ResourceSnapshot {
                    process_count: 1,
                    process_count_value_state: crate::state::ResourceMetricValueState::Observed,
                    process_ids: vec![sample_fence.root().id().pid()],
                    managed_process_fence: Some(sample_fence),
                    metrics_status: crate::domain::snapshot::ProcessMetricStatus::Complete,
                    metric_values: crate::state::ResourceMetricValueState::Observed,
                    cpu_value_state: crate::state::ResourceMetricValueState::Observed,
                    memory_value_state: crate::state::ResourceMetricValueState::Observed,
                    ..ResourceSnapshot::default()
                },
                false,
                std::time::Instant::now() + Duration::from_secs(10),
            )
        });

        validated_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("sample reached post-validation barrier");
        teardown
            .request_close()
            .expect("request asynchronous coordinator close");
        release_attempted_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("coordinator reached exact registry release");
        release_resume_tx
            .send(())
            .expect("allow exact registry release attempt");

        let released_while_publication_paused =
            released_rx.recv_timeout(Duration::from_millis(200)).is_ok();
        let registry_current_while_publication_paused = teardown
            .exact_registry_entry_is_current_until(
                std::time::Instant::now() + Duration::from_secs(1),
            )
            .unwrap_or(false);
        publication_resume_tx
            .send(())
            .expect("release sample publication");
        let publication = sampler.join().expect("join sample publication");
        if !released_while_publication_paused {
            released_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("registry releases after sample commit");
        }
        session.close(false).expect("settle managed teardown");

        assert!(
            !released_while_publication_paused,
            "registry release crossed the validated-but-uncommitted sample"
        );
        assert!(
            registry_current_while_publication_paused,
            "validated sample lost exact registry authority before commit"
        );
        assert!(
            matches!(
                &publication,
                Ok(ManagedResourceSamplePublication::Published { .. })
                    | Ok(ManagedResourceSamplePublication::StaleGeneration { .. })
            ),
            "validated sample did not resolve safely: {publication:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn exact_close_retires_generation_before_a_stale_arc_can_restart_it() {
        let runtime = Arc::new(RwLock::new(RuntimeState::default()));
        let journal = tempfile::tempdir().expect("terminal retirement journal");
        let resource_id = ResourceId::new();
        let completion_store =
            TeardownCompletionStore::durable(journal.path().join("teardown.sqlite3"))
                .expect("durable terminal teardown store");
        let authority = TerminalLaunchAuthority::new(
            ProcessOwner::Host,
            resource_id,
            1,
            OperationId::new(),
            1,
            Vec::new(),
            completion_store.clone(),
        )
        .expect("terminal launch authority");
        let session = spawn_with_command(
            "exact-close-retirement-test",
            std::env::current_dir().expect("test cwd"),
            SessionDimensions::default(),
            "cmd.exe".to_string(),
            vec![
                "/d".to_string(),
                "/s".to_string(),
                "/c".to_string(),
                "ping -n 30 127.0.0.1 >nul".to_string(),
            ],
            HashMap::new(),
            100,
            None,
            runtime,
            false,
            TerminalBackend::PortablePtyFeedingAlacritty,
            false,
            None,
            None,
            authority,
        )
        .expect("spawn managed terminal");
        let fence = session
            .managed_process_snapshot()
            .expect("exact managed process snapshot")
            .0;

        session
            .close_managed_process_exact(&fence, true)
            .expect("exact generation close");
        session
            .close_managed_process_exact(&fence, true)
            .expect("an exact completed-generation retry must be idempotent");

        let restart_authority = TerminalLaunchAuthority::new(
            ProcessOwner::Host,
            resource_id,
            2,
            OperationId::new(),
            2,
            Vec::new(),
            completion_store,
        )
        .expect("terminal restart authority");
        let error = session
            .restart_command(
                std::env::current_dir().expect("restart test cwd"),
                SessionDimensions::default(),
                "cmd.exe".to_string(),
                vec!["/d".to_string(), "/c".to_string(), "exit 0".to_string()],
                HashMap::new(),
                None,
                false,
                restart_authority,
            )
            .expect_err("a stale Arc must not restart a retired terminal generation");
        assert!(error.contains("retired"), "unexpected error: {error}");
    }

    #[cfg(windows)]
    #[test]
    fn production_terminal_drop_closes_job_and_joins_real_pty_actors() {
        let runtime = Arc::new(RwLock::new(RuntimeState::default()));
        let journal = tempfile::tempdir().expect("terminal drop teardown journal");
        let authority = TerminalLaunchAuthority::new(
            ProcessOwner::Host,
            ResourceId::new(),
            1,
            OperationId::new(),
            1,
            Vec::new(),
            TeardownCompletionStore::durable(journal.path().join("teardown.sqlite3"))
                .expect("durable terminal teardown store"),
        )
        .expect("terminal launch authority");
        let session = spawn_with_command(
            "suspended-managed-terminal-drop-test",
            std::env::current_dir().expect("test cwd"),
            SessionDimensions::default(),
            "cmd.exe".to_string(),
            vec![
                "/d".to_string(),
                "/s".to_string(),
                "/c".to_string(),
                "ping -n 30 127.0.0.1 >nul".to_string(),
            ],
            HashMap::new(),
            100,
            None,
            runtime,
            false,
            TerminalBackend::PortablePtyFeedingAlacritty,
            false,
            None,
            None,
            authority,
        )
        .expect("spawn production terminal for drop");
        let pid = session
            .managed_process_snapshot()
            .expect("drop-test managed process snapshot")
            .0
            .root()
            .id()
            .pid();

        drop(session);

        assert!(
            !platform_service::is_pid_running(pid),
            "TerminalSession::drop must synchronously close its Job-owned root"
        );
    }

    #[cfg(windows)]
    #[test]
    fn wait_actor_setup_failure_joins_existing_reader_and_closes_managed_root() {
        let runtime = Arc::new(RwLock::new(RuntimeState::default()));
        let temp = tempfile::tempdir().expect("terminal actor failure temp dir");
        let _pid_file_guard = pid_file::use_test_pid_file(temp.path().join("running-pids.json"));
        let authority = TerminalLaunchAuthority::new(
            ProcessOwner::Host,
            ResourceId::new(),
            1,
            OperationId::new(),
            1,
            Vec::new(),
            TeardownCompletionStore::durable(temp.path().join("teardown.sqlite3"))
                .expect("durable terminal teardown store"),
        )
        .expect("terminal launch authority");
        FAIL_NEXT_WAIT_ACTOR_SPAWN.store(true, Ordering::SeqCst);

        let error = match spawn_with_command(
            "managed-terminal-wait-actor-failure",
            std::env::current_dir().expect("test cwd"),
            SessionDimensions::default(),
            "cmd.exe".to_string(),
            vec![
                "/d".to_string(),
                "/s".to_string(),
                "/c".to_string(),
                "ping -n 30 127.0.0.1 >nul".to_string(),
            ],
            HashMap::new(),
            100,
            None,
            runtime,
            false,
            TerminalBackend::PortablePtyFeedingAlacritty,
            true,
            None,
            None,
            authority,
        ) {
            Ok(session) => {
                drop(session);
                panic!("injected wait actor spawn must fail");
            }
            Err(error) => error,
        };

        assert!(
            error.contains("injected wait actor spawn failure"),
            "{error}"
        );
        assert!(
            pid_file::active_tracked_processes_for_session(
                "managed-terminal-wait-actor-failure"
            )
            .is_empty(),
            "setup failure must not return until its Job root is zero and its ledger settlement is released"
        );
    }

    #[cfg(windows)]
    #[test]
    fn input_admission_setup_failure_joins_both_actors_and_closes_managed_root() {
        let runtime = Arc::new(RwLock::new(RuntimeState::default()));
        let temp = tempfile::tempdir().expect("terminal input failure temp dir");
        let _pid_file_guard = pid_file::use_test_pid_file(temp.path().join("running-pids.json"));
        let authority = TerminalLaunchAuthority::new(
            ProcessOwner::Host,
            ResourceId::new(),
            1,
            OperationId::new(),
            1,
            Vec::new(),
            TeardownCompletionStore::durable(temp.path().join("teardown.sqlite3"))
                .expect("durable terminal teardown store"),
        )
        .expect("terminal launch authority");
        FAIL_NEXT_INPUT_ADMISSION_OPEN.store(true, Ordering::SeqCst);

        let error = match spawn_with_command(
            "managed-terminal-input-admission-failure",
            std::env::current_dir().expect("test cwd"),
            SessionDimensions::default(),
            "cmd.exe".to_string(),
            vec![
                "/d".to_string(),
                "/s".to_string(),
                "/c".to_string(),
                "ping -n 30 127.0.0.1 >nul".to_string(),
            ],
            HashMap::new(),
            100,
            None,
            runtime,
            false,
            TerminalBackend::PortablePtyFeedingAlacritty,
            true,
            None,
            None,
            authority,
        ) {
            Ok(session) => {
                drop(session);
                panic!("injected input-admission open must fail");
            }
            Err(error) => error,
        };

        assert!(
            error.contains("injected terminal input-admission open failure"),
            "{error}"
        );
        assert!(
            pid_file::active_tracked_processes_for_session(
                "managed-terminal-input-admission-failure"
            )
            .is_empty(),
            "setup failure must join reader/wait actors, prove Job zero, and release its ledger observation"
        );
    }

    fn test_event_proxy(dimensions: SessionDimensions) -> SessionEventProxy {
        SessionEventProxy {
            session_id: "test".to_string(),
            writer: Arc::new(Mutex::new(Box::new(io::sink()) as Box<dyn Write + Send>)),
            input_admission: Arc::new(AtomicBool::new(true)),
            runtime_state: Arc::new(RwLock::new(RuntimeState::default())),
            dimensions: Arc::new(Mutex::new(dimensions)),
            debug_enabled: false,
            state_notifier: None,
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
    fn bash_shell_args_returns_login_flag_when_integration_disabled() {
        let args = bash_shell_args(false);
        assert_eq!(args, vec!["--login"]);
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
    fn paste_source_limit_rejects_escape_only_input_before_sanitizing() {
        let source = "\u{1b}".repeat(MAX_TERMINAL_INPUT_BYTES + 1);

        let error = prepare_paste_payload("", &source, true)
            .expect_err("source bytes must be bounded before bracketed-paste sanitization");

        assert!(error.contains("PTY input exceeds"), "{error}");
    }

    #[test]
    fn paste_source_limit_includes_user_prefix_before_building_payload() {
        let source = "x".repeat(MAX_TERMINAL_INPUT_BYTES);

        let error = prepare_paste_payload("p", &source, false)
            .expect_err("prefix and user input must share the source byte bound");

        assert!(error.contains("PTY input exceeds"), "{error}");
    }

    #[test]
    fn plain_paste_normalizes_newlines_to_carriage_returns() {
        let normalized = normalize_plain_paste_text("one\r\ntwo\nthree");

        assert_eq!(normalized, "one\rtwo\rthree");
    }

    #[derive(Default)]
    struct CountingWriteState {
        bytes: Vec<u8>,
        writes: usize,
        flushes: usize,
        fail_write: bool,
        fail_flush: bool,
    }

    struct CountingWriter(Arc<Mutex<CountingWriteState>>);

    impl Write for CountingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut state = self.0.lock().unwrap();
            state.writes += 1;
            if state.fail_write {
                return Err(io::Error::other("write failed"));
            }
            state.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            let mut state = self.0.lock().unwrap();
            state.flushes += 1;
            if state.fail_flush {
                return Err(io::Error::other("flush failed"));
            }
            Ok(())
        }
    }

    fn counting_writer(state: Arc<Mutex<CountingWriteState>>) -> Arc<Mutex<Box<dyn Write + Send>>> {
        Arc::new(Mutex::new(Box::new(CountingWriter(state))))
    }

    #[test]
    fn composite_user_payload_is_one_write_and_one_flush() {
        let state = Arc::new(Mutex::new(CountingWriteState::default()));
        let writer = counting_writer(state.clone());
        let admission = AtomicBool::new(true);

        write_composite_pty_payload(
            &writer,
            &admission,
            b"annotation preamble\n",
            b"user prompt",
        )
        .unwrap();

        let state = state.lock().unwrap();
        assert_eq!(state.bytes, b"annotation preamble\nuser prompt");
        assert_eq!(state.writes, 1);
        assert_eq!(state.flushes, 1);
    }

    #[test]
    fn provider_payload_writes_without_waiting_for_conpty_flush() {
        let state = Arc::new(Mutex::new(CountingWriteState {
            fail_flush: true,
            ..CountingWriteState::default()
        }));
        let writer = counting_writer(state.clone());
        let admission = AtomicBool::new(true);

        write_composite_pty_payload_inner(&writer, &admission, b"", b"provider prompt", false)
            .expect("provider write must not call the blocking flush path");

        let state = state.lock().unwrap();
        assert_eq!(state.bytes, b"provider prompt");
        assert_eq!(state.writes, 1);
        assert_eq!(state.flushes, 0);
    }

    #[test]
    fn composite_bracketed_paste_wraps_only_the_user_text() {
        let payload = composite_paste_payload("annotation preamble\n", "hello\u{1b}world", true);

        assert_eq!(
            payload,
            b"annotation preamble\n\x1b[200~helloworld\x1b[201~"
        );
    }

    #[test]
    fn composite_write_and_flush_failures_are_reported() {
        let write_state = Arc::new(Mutex::new(CountingWriteState {
            fail_write: true,
            ..CountingWriteState::default()
        }));
        let admission = AtomicBool::new(true);
        let error = write_composite_pty_payload(
            &counting_writer(write_state),
            &admission,
            b"prefix",
            b"input",
        )
        .unwrap_err();
        assert!(error.contains("write"));

        let flush_state = Arc::new(Mutex::new(CountingWriteState {
            fail_flush: true,
            ..CountingWriteState::default()
        }));
        let error = write_composite_pty_payload(
            &counting_writer(flush_state),
            &admission,
            b"prefix",
            b"input",
        )
        .unwrap_err();
        assert!(error.contains("flush"));
    }

    #[test]
    fn terminal_input_is_rejected_after_teardown_admission_closes() {
        let state = Arc::new(Mutex::new(CountingWriteState::default()));
        let writer = counting_writer(state.clone());
        let admission = AtomicBool::new(false);

        let error = write_composite_pty_payload(&writer, &admission, b"prefix", b"input")
            .expect_err("closed input admission must reject every later writer");

        assert!(error.contains("closed"));
        assert!(state.lock().unwrap().bytes.is_empty());
    }

    #[test]
    fn terminal_input_drain_revokes_admission_before_process_termination() {
        let state = Arc::new(Mutex::new(CountingWriteState::default()));
        let writer = counting_writer(state.clone());
        let admission = AtomicBool::new(true);
        let deadline = std::time::Instant::now()
            .checked_add(Duration::from_secs(1))
            .expect("input-drain test deadline");

        drain_terminal_input_until(&admission, &writer, deadline)
            .expect("input drain must settle while the writer is idle");

        assert!(!admission.load(Ordering::Acquire));
        let error = write_composite_pty_payload(&writer, &admission, b"", b"late input")
            .expect_err("no writer may pass admission after the drain returns");
        assert!(error.contains("closed"));
        assert!(state.lock().unwrap().bytes.is_empty());
    }

    #[cfg(not(windows))]
    #[test]
    fn actor_shutdown_fails_boundedly_when_writer_lock_is_unjoinable() {
        let writer = Arc::new(Mutex::new(Box::new(io::sink()) as Box<dyn Write + Send>));
        let master = Arc::new(Mutex::new(None));
        let actors = Arc::new(Mutex::new(TerminalActorHandles::default()));
        let input_admission = AtomicBool::new(true);
        let writer_guard = writer.lock().expect("writer lock");
        let started = std::time::Instant::now();

        let result = detach_pty_and_join_actor_slots(&input_admission, &writer, &master, &actors);

        drop(writer_guard);
        assert!(
            result
                .expect_err("a contended writer must fail closed")
                .contains("writer"),
            "shutdown must identify the unjoinable writer lock"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "unjoinable actor shutdown must be bounded"
        );
    }

    #[test]
    fn truncate_utf8_boundary_does_not_split_multibyte_chars() {
        let text = "a😀b";

        assert_eq!(truncate_utf8_boundary(text, 2), "a");
        assert_eq!(truncate_utf8_boundary(text, 5), "a😀");
    }
}
