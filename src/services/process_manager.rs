use crate::models::{
    Project, ProjectFolder, RunCommand, SSHConnection, SessionTab, Settings, TabType,
};
use crate::notifications;
use crate::services::{env_service, pid_file, platform_service};
use crate::state::AppState;
use crate::state::{
    AiIdleTransition, AiLaunchSpec, ResourceSnapshot, RuntimeState, ServerLaunchSpec,
    SessionDimensions, SessionExitState, SessionKind, SessionRuntimeState, SessionStatus,
    SshLaunchSpec,
};
use crate::terminal::session::{
    preferred_windows_bash_program, TerminalBackend, TerminalSession, TerminalSessionView,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, RwLock,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct ProcessManager {
    inner: Arc<ProcessManagerInner>,
}

struct ProcessManagerInner {
    sessions: Mutex<HashMap<String, Arc<TerminalSession>>>,
    runtime_state: Arc<RwLock<RuntimeState>>,
    settings: RwLock<Settings>,
    terminal_backend: TerminalBackend,
    debug_enabled: bool,
    restart_backoffs: Mutex<HashMap<String, RestartBackoff>>,
    notification_sound: RwLock<Option<String>>,
    scrollback_lines: RwLock<usize>,
    background_stop: AtomicBool,
    background_thread: Mutex<Option<thread::JoinHandle<()>>>,
}

#[derive(Debug, Clone)]
struct RestartBackoff {
    delay: Duration,
    last_crash: Instant,
}

#[derive(Debug, Clone, Default)]
pub struct AiRestoreReport {
    pub reattached: usize,
    pub relaunched: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SshRestoreReport {
    pub reattached: usize,
    pub recovered: usize,
    pub disconnected: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ManagedShutdownReport {
    pub requested_sessions: usize,
    pub forced_kill_pids: usize,
    pub remaining_live_sessions: usize,
    pub remaining_tracked_pids: usize,
}

static AI_SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);
static SSH_SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

const DEFAULT_CLAUDE_COMMAND: &str =
    "npx -y @anthropic-ai/claude-code@latest --dangerously-skip-permissions";
const DEFAULT_CODEX_COMMAND: &str =
    "npx -y @openai/codex@latest --dangerously-bypass-approvals-and-sandbox";
const AI_COMMAND_INJECTION_DELAY_MS: u64 = 500;

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessManager {
    pub fn new() -> Self {
        let debug_enabled = debug_enabled();
        let inner = Arc::new(ProcessManagerInner {
            sessions: Mutex::new(HashMap::new()),
            runtime_state: Arc::new(RwLock::new(RuntimeState::new(debug_enabled))),
            settings: RwLock::new(Settings::default()),
            terminal_backend: TerminalBackend::PortablePtyFeedingAlacritty,
            debug_enabled,
            restart_backoffs: Mutex::new(HashMap::new()),
            notification_sound: RwLock::new(None),
            scrollback_lines: RwLock::new(10_000),
            background_stop: AtomicBool::new(false),
            background_thread: Mutex::new(None),
        });

        let thread_handle = spawn_background_tasks(inner.clone());
        if let Ok(mut handle_slot) = inner.background_thread.lock() {
            *handle_slot = Some(thread_handle);
        }

        Self { inner }
    }

    pub fn runtime_state(&self) -> RuntimeState {
        self.inner
            .runtime_state
            .read()
            .map(|runtime| runtime.clone())
            .unwrap_or_default()
    }

    pub fn register_runtime_session(&self, session: SessionRuntimeState) {
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            runtime.sessions.insert(session.session_id.clone(), session);
        }
    }

    pub fn terminal_backend(&self) -> TerminalBackend {
        self.inner.terminal_backend
    }

    pub fn debug_enabled(&self) -> bool {
        self.inner.debug_enabled
    }

    pub fn set_notification_sound(&self, sound_id: Option<String>) {
        if let Ok(mut notification_sound) = self.inner.notification_sound.write() {
            *notification_sound = sound_id;
        }
    }

    pub fn set_settings(&self, settings: Settings) {
        if let Ok(mut settings_slot) = self.inner.settings.write() {
            *settings_slot = settings;
        }
    }

    pub fn set_log_buffer_size(&self, lines: usize) {
        let lines = lines.max(100);
        if let Ok(mut scrollback_lines) = self.inner.scrollback_lines.write() {
            *scrollback_lines = lines;
        }
        if let Ok(sessions) = self.inner.sessions.lock() {
            for session in sessions.values() {
                session.set_scrollback_lines(lines);
            }
        }
    }

    fn log_buffer_size(&self) -> usize {
        self.inner
            .scrollback_lines
            .read()
            .map(|lines| *lines)
            .unwrap_or(10_000)
    }

    pub fn set_active_session(&self, session_id: impl Into<String>) {
        let session_id = session_id.into();
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            runtime.active_session_id = Some(session_id.clone());
            if let Some(session) = runtime.sessions.get_mut(&session_id) {
                session.clear_unseen_ready();
            }
        }
    }

    pub fn spawn_shell_session(
        &self,
        session_id: impl Into<String>,
        cwd: &Path,
        dimensions: SessionDimensions,
        default_terminal: Option<crate::models::DefaultTerminal>,
    ) -> Result<(), String> {
        let session_id = session_id.into();
        self.set_active_session(session_id.clone());

        if self.session_exists(&session_id) {
            return Ok(());
        }

        match TerminalSession::spawn(
            session_id.clone(),
            cwd.to_path_buf(),
            dimensions,
            default_terminal,
            self.log_buffer_size(),
            self.inner.runtime_state.clone(),
            self.inner.debug_enabled,
        ) {
            Ok(session) => {
                self.inner
                    .sessions
                    .lock()
                    .map_err(|_| "Session store poisoned".to_string())?
                    .insert(session_id, Arc::new(session));
                Ok(())
            }
            Err(error) => {
                self.update_session_state(&session_id, |state| {
                    state.cwd = cwd.to_path_buf();
                    state.dimensions = dimensions;
                    state.status = SessionStatus::Failed;
                    state.exit = Some(SessionExitState {
                        code: None,
                        signal: None,
                        closed_by_user: false,
                        summary: error.clone(),
                    });
                    state.mark_dirty();
                });
                Err(error)
            }
        }
    }

    pub fn write_to_session(&self, session_id: &str, text: &str) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.write_text(text)
    }

    pub fn write_bytes_to_session(&self, session_id: &str, bytes: &[u8]) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.write_bytes(bytes)
    }

    pub fn paste_to_session(&self, session_id: &str, text: &str) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.paste_text(text)
    }

    pub fn write_virtual_text(&self, session_id: &str, text: &str) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.write_virtual_text(text);
        Ok(())
    }

    pub fn clear_virtual_output(&self, session_id: &str) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.clear_virtual_output();
        self.update_session_state(session_id, |state| {
            state.display_offset = 0;
            state.mark_dirty();
        });
        Ok(())
    }

    pub fn note_server_interrupt(&self, session_id: &str) {
        self.update_session_state(session_id, |state| {
            if matches!(state.session_kind, SessionKind::Server)
                && state.status.is_live()
                && !state.interactive_shell
            {
                state.note_user_interrupt();
            }
        });
    }

    pub fn report_focus(&self, session_id: &str, focused: bool) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.report_focus(focused)
    }

    pub fn resize_session(
        &self,
        session_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<(), String> {
        let current_dimensions = self
            .runtime_state()
            .sessions
            .get(session_id)
            .map(|session| session.dimensions)
            .unwrap_or_default();

        if current_dimensions == dimensions {
            return Ok(());
        }

        let session = self.get_session(session_id)?;
        session.resize(dimensions)
    }

    pub fn scroll_session(&self, session_id: &str, delta_lines: i32) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.scroll(delta_lines)
    }

    pub fn close_session(&self, session_id: &str) -> Result<(), String> {
        self.request_session_close(session_id, true)
    }

    pub fn active_session(&self) -> Option<TerminalSessionView> {
        let runtime = self.runtime_state();
        let active_id = runtime.active_session_id?;
        let runtime_session = runtime.sessions.get(&active_id)?.clone();
        let session = self.get_session(&active_id).ok()?;

        Some(TerminalSessionView {
            runtime: runtime_session,
            screen: session.snapshot(),
        })
    }

    pub fn session_view(&self, session_id: &str) -> Option<TerminalSessionView> {
        let runtime = self.runtime_state();
        let runtime_session = runtime.sessions.get(session_id)?.clone();
        let session = self.get_session(session_id).ok()?;

        Some(TerminalSessionView {
            runtime: runtime_session,
            screen: session.snapshot(),
        })
    }

    pub fn record_frame(&self, session_id: &str, render_duration: Duration) {
        let render_micros = render_duration.as_micros() as u64;
        self.update_session_state(session_id, |state| state.record_frame(render_micros));
    }

    pub fn start_ai_session(
        &self,
        app_state: &mut AppState,
        project_id: &str,
        tab_type: TabType,
        dimensions: SessionDimensions,
    ) -> Result<String, String> {
        if app_state.find_project(project_id).is_none() {
            return Err(format!("Unknown project `{project_id}`"));
        }
        let label = app_state.next_ai_label(project_id, tab_type.clone());
        let session_id = next_ai_session_id(&tab_type);
        let tab_id = session_id.clone();

        app_state.open_ai_tab(
            project_id,
            tab_type,
            tab_id.clone(),
            session_id,
            Some(label),
        );

        self.ensure_ai_session_for_tab(app_state, &tab_id, dimensions, true, false)
    }

    pub fn ensure_ai_session_for_tab(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        force_new_session: bool,
    ) -> Result<String, String> {
        let tab = app_state
            .find_ai_tab(tab_id)
            .cloned()
            .ok_or_else(|| format!("Unknown AI tab `{tab_id}`"))?;

        let project = app_state
            .find_project(&tab.project_id)
            .cloned()
            .ok_or_else(|| format!("Unknown project `{}`", tab.project_id))?;

        if let Some(existing_session_id) = tab.pty_session_id.as_deref() {
            let session_live = self
                .runtime_state()
                .sessions
                .get(existing_session_id)
                .map(|session| session.status.is_live())
                .unwrap_or(false)
                && self.get_session(existing_session_id).is_ok();
            if session_live && !force_new_session {
                if activate_tab {
                    let _ = app_state.select_tab(&tab.id);
                    self.set_active_session(existing_session_id.to_string());
                }
                return Ok(existing_session_id.to_string());
            }
            self.forget_session(existing_session_id);
        }

        let session_id = next_ai_session_id(&tab.tab_type);
        let launch = build_ai_launch_spec(&app_state.config.settings, &project, &tab, &session_id)?;

        let _ = app_state.update_ai_tab_session(&tab.id, session_id.clone());
        if activate_tab {
            let _ = app_state.select_tab(&tab.id);
        }

        self.ensure_runtime_entry(&session_id, launch.cwd.clone(), dimensions);
        self.update_session_state(&session_id, |state| {
            state.status = SessionStatus::Starting;
            state.cwd = launch.cwd.clone();
            state.dimensions = dimensions;
            state.shell_program = launch.shell_program.clone();
            state.configure_ai(launch.clone());
            state.exit = None;
        });

        self.spawn_ai_shell_session(&launch, &session_id, dimensions)?;
        if activate_tab {
            self.set_active_session(session_id.clone());
        }
        Ok(session_id)
    }

    pub fn restart_ai_session(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<String, String> {
        let existing_session_id = app_state
            .find_ai_tab(tab_id)
            .and_then(|tab| tab.pty_session_id.clone());
        if let Some(session_id) = existing_session_id {
            let _ = self.close_session(&session_id);
            self.forget_session(&session_id);
        }

        self.ensure_ai_session_for_tab(app_state, tab_id, dimensions, true, true)
    }

    pub fn close_ai_session(&self, app_state: &mut AppState, tab_id: &str) -> Result<(), String> {
        let session_id = app_state
            .find_ai_tab(tab_id)
            .and_then(|tab| tab.pty_session_id.clone());

        if let Some(session_id) = session_id {
            let _ = self.close_session(&session_id);
            self.forget_session(&session_id);
        }
        app_state.remove_tab(tab_id);
        Ok(())
    }

    pub fn reconcile_saved_ai_tabs(&self, app_state: &mut AppState) -> usize {
        let runtime = self.runtime_state();
        let mut recovered = Vec::new();
        let existing_ids: std::collections::HashSet<String> = app_state
            .open_tabs
            .iter()
            .map(|tab| tab.id.clone())
            .collect();

        for session in runtime.sessions.values() {
            if !session.session_kind.is_ai() || !session.status.is_live() {
                continue;
            }

            let Some(tab_id) = session.tab_id.as_ref() else {
                continue;
            };
            if existing_ids.contains(tab_id) {
                continue;
            }

            let tab_type = match session.session_kind {
                SessionKind::Claude => TabType::Claude,
                SessionKind::Codex => TabType::Codex,
                _ => continue,
            };
            let label = session
                .title
                .clone()
                .unwrap_or_else(|| default_ai_label(tab_type.clone()));

            recovered.push(SessionTab {
                id: tab_id.clone(),
                tab_type,
                project_id: session.project_id.clone().unwrap_or_default(),
                command_id: None,
                pty_session_id: Some(session.session_id.clone()),
                label: Some(label),
                ssh_connection_id: None,
            });
        }

        app_state.merge_recovered_ai_tabs(recovered)
    }

    pub fn restore_ai_tabs(
        &self,
        app_state: &mut AppState,
        dimensions: SessionDimensions,
    ) -> AiRestoreReport {
        let mut report = AiRestoreReport::default();
        let active_tab_id = app_state.active_tab_id.clone();

        let saved_ai_tabs: Vec<String> = app_state.ai_tabs().map(|tab| tab.id.clone()).collect();
        for tab_id in saved_ai_tabs {
            let live_session_for_tab = self.runtime_state().sessions.values().find_map(|session| {
                (session.session_kind.is_ai()
                    && session.status.is_live()
                    && session.tab_id.as_deref() == Some(tab_id.as_str()))
                .then(|| session.session_id.clone())
            });
            if let Some(session_id) = live_session_for_tab {
                let _ = app_state.update_ai_tab_session(&tab_id, session_id);
                report.reattached += 1;
                continue;
            }

            let live_session = app_state
                .find_ai_tab(&tab_id)
                .and_then(|tab| tab.pty_session_id.as_deref())
                .and_then(|session_id| self.runtime_state().sessions.get(session_id).cloned())
                .map(|session| session.status.is_live())
                .unwrap_or(false);

            if live_session {
                report.reattached += 1;
                continue;
            }

            match self.ensure_ai_session_for_tab(app_state, &tab_id, dimensions, false, true) {
                Ok(_) => report.relaunched += 1,
                Err(_) => report.failed += 1,
            }
        }

        let recovered = self.reconcile_saved_ai_tabs(app_state);
        report.reattached += recovered;

        app_state.active_tab_id = active_tab_id
            .filter(|tab_id| app_state.find_tab(tab_id).is_some())
            .or_else(|| app_state.open_tabs.first().map(|tab| tab.id.clone()));

        report
    }

    pub fn start_ssh_session(
        &self,
        app_state: &mut AppState,
        connection_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<String, String> {
        let connection = app_state
            .find_ssh_connection(connection_id)
            .cloned()
            .ok_or_else(|| format!("Unknown SSH connection `{connection_id}`"))?;
        let project_id = app_state
            .find_ssh_tab_by_connection(connection_id)
            .map(|tab| tab.project_id.clone())
            .or_else(|| app_state.active_project().map(|project| project.id.clone()))
            .or_else(|| {
                app_state
                    .projects()
                    .first()
                    .map(|project| project.id.clone())
            })
            .unwrap_or_default();
        let tab_id = app_state.open_ssh_tab(&project_id, connection_id, Some(connection.label));

        self.ensure_ssh_session_for_tab(app_state, &tab_id, dimensions, true, false)
    }

    pub fn ensure_ssh_session_for_tab(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        force_new_session: bool,
    ) -> Result<String, String> {
        let tab = app_state
            .find_ssh_tab(tab_id)
            .cloned()
            .ok_or_else(|| format!("Unknown SSH tab `{tab_id}`"))?;
        let connection_id = tab
            .ssh_connection_id
            .clone()
            .ok_or_else(|| format!("SSH tab `{tab_id}` is missing a connection id"))?;
        let connection = app_state
            .find_ssh_connection(&connection_id)
            .cloned()
            .ok_or_else(|| format!("Unknown SSH connection `{connection_id}`"))?;

        if let Some(existing_session_id) = tab.pty_session_id.as_deref() {
            let session_live = self
                .runtime_state()
                .sessions
                .get(existing_session_id)
                .map(|session| {
                    session.status.is_live() && matches!(session.session_kind, SessionKind::Ssh)
                })
                .unwrap_or(false)
                && self.get_session(existing_session_id).is_ok();
            if session_live && !force_new_session {
                if activate_tab {
                    let _ = app_state.select_tab(&tab.id);
                    self.set_active_session(existing_session_id.to_string());
                }
                return Ok(existing_session_id.to_string());
            }
            self.forget_session(existing_session_id);
        }

        let session_id = next_ssh_session_id(&connection_id);
        let launch = build_ssh_launch_spec(app_state, &tab, &connection);

        let _ = app_state.update_ssh_tab_session(&tab.id, Some(session_id.clone()));
        if activate_tab {
            let _ = app_state.select_tab(&tab.id);
        }

        self.ensure_runtime_entry(&session_id, launch.cwd.clone(), dimensions);
        self.update_session_state(&session_id, |state| {
            state.status = SessionStatus::Starting;
            state.cwd = launch.cwd.clone();
            state.dimensions = dimensions;
            state.shell_program = launch.program.clone();
            state.configure_ssh(launch.clone());
            state.exit = None;
        });

        self.spawn_ssh_session(&launch, &session_id, dimensions)?;
        if activate_tab {
            self.set_active_session(session_id.clone());
        }
        Ok(session_id)
    }

    pub fn restart_ssh_session(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<String, String> {
        let existing_session_id = app_state
            .find_ssh_tab(tab_id)
            .and_then(|tab| tab.pty_session_id.clone());
        if let Some(session_id) = existing_session_id {
            let _ = self.close_session(&session_id);
            self.forget_session(&session_id);
            let _ = app_state.update_ssh_tab_session(tab_id, None);
        }

        self.ensure_ssh_session_for_tab(app_state, tab_id, dimensions, true, true)
    }

    pub fn close_ssh_session(&self, app_state: &mut AppState, tab_id: &str) -> Result<(), String> {
        let session_id = app_state
            .find_ssh_tab(tab_id)
            .and_then(|tab| tab.pty_session_id.clone());

        if let Some(session_id) = session_id {
            let _ = self.close_session(&session_id);
            self.forget_session(&session_id);
        }
        let _ = app_state.update_ssh_tab_session(tab_id, None);
        Ok(())
    }

    pub fn reconcile_saved_ssh_tabs(&self, app_state: &mut AppState) -> usize {
        let runtime = self.runtime_state();
        let mut recovered = Vec::new();
        let existing_ids: std::collections::HashSet<String> = app_state
            .open_tabs
            .iter()
            .map(|tab| tab.id.clone())
            .collect();

        for session in runtime.sessions.values() {
            if !matches!(session.session_kind, SessionKind::Ssh) || !session.status.is_live() {
                continue;
            }

            let Some(tab_id) = session.tab_id.as_ref() else {
                continue;
            };
            if existing_ids.contains(tab_id) {
                continue;
            }

            let Some(connection_id) = session
                .ssh_launch
                .as_ref()
                .map(|launch| launch.ssh_connection_id.clone())
            else {
                continue;
            };
            let Some(connection) = app_state.find_ssh_connection(&connection_id) else {
                continue;
            };

            recovered.push(SessionTab {
                id: tab_id.clone(),
                tab_type: TabType::Ssh,
                project_id: session.project_id.clone().unwrap_or_default(),
                command_id: None,
                pty_session_id: Some(session.session_id.clone()),
                label: Some(connection.label.clone()),
                ssh_connection_id: Some(connection_id),
            });
        }

        app_state.merge_recovered_ssh_tabs(recovered)
    }

    pub fn restore_ssh_tabs(&self, app_state: &mut AppState) -> SshRestoreReport {
        let mut report = SshRestoreReport::default();
        let active_tab_id = app_state.active_tab_id.clone();

        let saved_ssh_tabs: Vec<String> = app_state.ssh_tabs().map(|tab| tab.id.clone()).collect();
        for tab_id in saved_ssh_tabs {
            let live_session_for_tab = self.runtime_state().sessions.values().find_map(|session| {
                (matches!(session.session_kind, SessionKind::Ssh)
                    && session.status.is_live()
                    && session.tab_id.as_deref() == Some(tab_id.as_str()))
                .then(|| session.session_id.clone())
            });
            if let Some(session_id) = live_session_for_tab {
                let _ = app_state.update_ssh_tab_session(&tab_id, Some(session_id));
                report.reattached += 1;
                continue;
            }

            let live_session = app_state
                .find_ssh_tab(&tab_id)
                .and_then(|tab| tab.pty_session_id.as_deref())
                .and_then(|session_id| self.runtime_state().sessions.get(session_id).cloned())
                .map(|session| {
                    session.status.is_live() && matches!(session.session_kind, SessionKind::Ssh)
                })
                .unwrap_or(false);

            if live_session {
                report.reattached += 1;
                continue;
            }

            let _ = app_state.update_ssh_tab_session(&tab_id, None);
            report.disconnected += 1;
        }

        report.recovered = self.reconcile_saved_ssh_tabs(app_state);
        app_state.active_tab_id = active_tab_id
            .filter(|tab_id| app_state.find_tab(tab_id).is_some())
            .or_else(|| app_state.open_tabs.first().map(|tab| tab.id.clone()));

        report
    }

    pub fn start_server(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<(), String> {
        self.start_server_with_activation(app_state, command_id, dimensions, true)
    }

    pub fn start_server_in_background(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<(), String> {
        self.start_server_with_activation(app_state, command_id, dimensions, false)
    }

    fn start_server_with_activation(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
    ) -> Result<(), String> {
        let lookup = app_state
            .find_command(command_id)
            .ok_or_else(|| format!("Unknown command `{command_id}`"))?;

        let project_id = lookup.project.id.clone();
        let command_id = lookup.command.id.clone();
        let command_label = lookup.command.label.clone();
        let command_auto_restart = lookup.command.auto_restart.unwrap_or(false);
        let session_id = command_id.clone();
        let runtime = self.runtime_state();
        if let Some(session) = runtime.sessions.get(&session_id) {
            if session.has_live_process() && self.get_session(&session_id).is_ok() {
                if activate_tab {
                    app_state.open_server_tab(
                        &project_id,
                        &command_id,
                        Some(command_label.clone()),
                    );
                    self.set_active_session(session_id);
                } else {
                    app_state.ensure_server_tab(
                        &project_id,
                        &command_id,
                        Some(command_label.clone()),
                    );
                }
                return Ok(());
            }
        }

        let previous_active_session_id = (!activate_tab)
            .then(|| runtime.active_session_id.clone())
            .flatten();

        if activate_tab {
            self.set_active_session(session_id.clone());
        }

        let cwd = PathBuf::from(lookup.folder.folder_path.clone());
        let cwd = if cwd.is_dir() {
            cwd
        } else {
            PathBuf::from(lookup.project.root_path.clone())
        };

        self.ensure_runtime_entry(&session_id, cwd.clone(), dimensions);

        let env = build_command_env(lookup.folder, lookup.command);
        let (program, args) =
            build_server_launch_command(&app_state.config.settings, lookup.command);
        let launch_spec = ServerLaunchSpec {
            command_id: command_id.clone(),
            project_id: project_id.clone(),
            cwd: cwd.clone(),
            program: program.clone(),
            args: args.clone(),
            env: env.clone(),
            auto_restart: command_auto_restart,
            log_file_path: build_server_log_file_path(
                lookup.project,
                lookup.folder,
                lookup.command,
            ),
        };

        if activate_tab {
            app_state.open_server_tab(&project_id, &command_id, Some(command_label.clone()));
        } else {
            app_state.ensure_server_tab(&project_id, &command_id, Some(command_label.clone()));
        }

        self.update_session_state(&session_id, |state| {
            state.status = SessionStatus::Starting;
            state.cwd = cwd.clone();
            state.dimensions = dimensions;
            state.shell_program = program.clone();
            state.configure_server(launch_spec.clone());
            state.exit = None;
            state.mark_dirty();
        });

        self.spawn_server_session(&launch_spec, dimensions, activate_tab)?;

        self.update_session_state(&session_id, |state| {
            state.configure_server(launch_spec.clone());
        });

        if !activate_tab {
            self.restore_active_session(previous_active_session_id);
        }

        Ok(())
    }

    pub fn stop_server(&self, command_id: &str) -> Result<(), String> {
        self.update_session_state(command_id, |state| {
            state.note_user_stop_request();
            state.status = SessionStatus::Stopping;
            state.exit = Some(SessionExitState {
                code: None,
                signal: None,
                closed_by_user: true,
                summary: "Server stop requested".to_string(),
            });
            state.mark_dirty();
        });
        self.close_session(command_id)
    }

    pub fn stop_server_and_wait(&self, command_id: &str, timeout: Duration) -> bool {
        let _ = self.stop_server(command_id);
        if self.wait_for_session_shutdown(command_id, timeout) {
            return true;
        }

        let _ = self.force_kill_session_processes(command_id);
        if self.wait_for_session_shutdown(command_id, Duration::from_secs(2)) {
            self.update_session_state(command_id, |state| {
                state.status = SessionStatus::Stopped;
                state.pid = None;
                state.resources = ResourceSnapshot::default();
                state.mark_dirty();
            });
            return true;
        }

        let remaining_tracked_pids = pid_file::active_tracked_pids_for_session(command_id);
        self.update_session_state(command_id, |state| {
            state.status = SessionStatus::Failed;
            state.pid = None;
            state.resources = ResourceSnapshot {
                process_count: remaining_tracked_pids.len() as u32,
                process_ids: remaining_tracked_pids.clone(),
                last_sample_at: Some(Instant::now()),
                ..ResourceSnapshot::default()
            };
            state.exit = Some(SessionExitState {
                code: None,
                signal: None,
                closed_by_user: true,
                summary: if remaining_tracked_pids.is_empty() {
                    "Managed process did not stop cleanly.".to_string()
                } else {
                    format!(
                        "Managed process left {} tracked child process(es) running.",
                        remaining_tracked_pids.len()
                    )
                },
            });
            state.mark_dirty();
        });
        false
    }

    pub fn restart_server(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<(), String> {
        self.restart_server_with_banner(app_state, command_id, dimensions, "--- Restarting... ---")
    }

    pub fn restart_server_with_banner(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        banner: &str,
    ) -> Result<(), String> {
        let lookup = app_state
            .find_command(command_id)
            .ok_or_else(|| format!("Unknown command `{command_id}`"))?;

        let project_id = lookup.project.id.clone();
        let command_id = lookup.command.id.clone();
        let command_label = lookup.command.label.clone();
        let command_auto_restart = lookup.command.auto_restart.unwrap_or(false);
        let clear_logs_on_restart = lookup.command.clear_logs_on_restart.unwrap_or(true);
        let cwd = PathBuf::from(lookup.folder.folder_path.clone());
        let cwd = if cwd.is_dir() {
            cwd
        } else {
            PathBuf::from(lookup.project.root_path.clone())
        };
        let env = build_command_env(lookup.folder, lookup.command);
        let (program, args) =
            build_server_launch_command(&app_state.config.settings, lookup.command);
        let launch_spec = ServerLaunchSpec {
            command_id: command_id.clone(),
            project_id: project_id.clone(),
            cwd: cwd.clone(),
            program: program.clone(),
            args: args.clone(),
            env: env.clone(),
            auto_restart: command_auto_restart,
            log_file_path: build_server_log_file_path(
                lookup.project,
                lookup.folder,
                lookup.command,
            ),
        };

        if !self.stop_server_and_wait(&command_id, Duration::from_secs(5)) {
            return Err(format!(
                "Managed process `{command_id}` did not stop cleanly."
            ));
        }
        self.set_active_session(command_id.clone());
        app_state.open_server_tab(&project_id, &command_id, Some(command_label));
        self.update_session_state(&command_id, |state| {
            state.status = SessionStatus::Starting;
            state.cwd = cwd.clone();
            state.dimensions = dimensions;
            state.shell_program = program.clone();
            state.configure_server(launch_spec.clone());
            state.exit = None;
            state.mark_dirty();
        });

        if let Ok(session) = self.get_session(&command_id) {
            if clear_logs_on_restart {
                session.clear_virtual_output();
            }
            session.write_virtual_text(&format!(
                "{}\x1b[33m{banner}\x1b[0m\r\n",
                if clear_logs_on_restart { "" } else { "\r\n" }
            ));
            session.restart_command(
                cwd.clone(),
                dimensions,
                program.clone(),
                args.clone(),
                env.clone(),
                launch_spec.log_file_path.clone(),
                true,
            )?;
            self.update_session_state(&command_id, |state| {
                state.configure_server(launch_spec.clone());
            });
            return Ok(());
        }

        self.start_server(app_state, &command_id, dimensions)?;
        let _ = self.write_virtual_text(
            &command_id,
            &format!(
                "{}\x1b[33m{banner}\x1b[0m\r\n",
                if clear_logs_on_restart { "" } else { "\r\n" }
            ),
        );
        Ok(())
    }

    pub fn start_all_for_project(
        &self,
        app_state: &mut AppState,
        project: &Project,
        dimensions: SessionDimensions,
    ) {
        for folder in &project.folders {
            for command in &folder.commands {
                let _ = self.start_server_in_background(app_state, &command.id, dimensions);
            }
        }
    }

    pub fn stop_all_for_project(&self, project_id: &str) {
        let runtime = self.runtime_state();
        for session in runtime.sessions.values() {
            if session.project_id.as_deref() == Some(project_id)
                && matches!(
                    session.status,
                    SessionStatus::Running | SessionStatus::Starting
                )
            {
                let _ = self.stop_server(&session.session_id);
            }
        }
    }

    pub fn stop_all_servers(&self) -> usize {
        let command_ids: Vec<String> = self
            .runtime_state()
            .sessions
            .values()
            .filter(|session| session.command_id.is_some() && session.status.is_live())
            .filter_map(|session| session.command_id.clone())
            .collect();

        for command_id in &command_ids {
            let _ = self.stop_server(command_id);
        }

        command_ids.len()
    }

    pub fn live_session_count(&self) -> usize {
        self.runtime_state()
            .sessions
            .values()
            .filter(|session| session.status.is_live())
            .count()
    }

    pub fn close_all_live_sessions(&self) -> usize {
        let session_ids = self.live_session_ids();

        for session_id in &session_ids {
            let _ = self.close_session(session_id);
        }

        session_ids.len()
    }

    pub fn shutdown_managed_processes(&self, timeout: Duration) -> ManagedShutdownReport {
        let session_ids = self.live_session_ids();
        for session_id in &session_ids {
            let _ = self.request_session_close(session_id, false);
        }

        let started_at = Instant::now();
        let mut active_tracked_processes = loop {
            let _ = pid_file::prune_inactive_entries();
            let remaining_live_sessions = self.live_session_count();
            let active_tracked_processes = pid_file::active_tracked_processes();
            if remaining_live_sessions == 0 && active_tracked_processes.is_empty() {
                break active_tracked_processes;
            }
            if started_at.elapsed() >= timeout {
                break active_tracked_processes;
            }
            thread::sleep(Duration::from_millis(100));
        };

        let mut forced_kill_pids = 0;
        if self.live_session_count() > 0 || !active_tracked_processes.is_empty() {
            let mut pids_to_kill = self.live_session_pids();
            pids_to_kill.extend(pid_file::active_tracked_pids());
            pids_to_kill.sort_unstable();
            pids_to_kill.dedup();

            for pid in pids_to_kill {
                if !platform_service::is_pid_running(pid) {
                    continue;
                }
                if platform_service::kill_process_tree(pid).is_ok()
                    || !platform_service::is_pid_running(pid)
                {
                    forced_kill_pids += 1;
                }
            }

            let _ = pid_file::prune_inactive_entries();
            let force_started = Instant::now();
            while force_started.elapsed() < Duration::from_secs(1) {
                let _ = pid_file::prune_inactive_entries();
                let remaining_live_sessions = self.live_session_count();
                active_tracked_processes = pid_file::active_tracked_processes();
                if remaining_live_sessions == 0 && active_tracked_processes.is_empty() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }

        let _ = pid_file::prune_inactive_entries();
        let report = ManagedShutdownReport {
            requested_sessions: session_ids.len(),
            forced_kill_pids,
            remaining_live_sessions: self.live_session_count(),
            remaining_tracked_pids: pid_file::active_tracked_pids().len(),
        };
        if report.remaining_live_sessions == 0 && report.remaining_tracked_pids == 0 {
            pid_file::clear_all();
        }
        report
    }

    pub fn reconcile_saved_server_tabs(&self, app_state: &mut AppState) -> usize {
        let runtime = self.runtime_state();
        let mut recovered = Vec::new();
        let existing_ids: std::collections::HashSet<String> = app_state
            .open_tabs
            .iter()
            .map(|tab| tab.id.clone())
            .collect();

        for session in runtime.sessions.values() {
            let Some(command_id) = session.command_id.as_deref() else {
                continue;
            };
            if !matches!(
                session.status,
                SessionStatus::Running | SessionStatus::Starting
            ) {
                continue;
            }
            if existing_ids.contains(command_id) {
                continue;
            }
            if let Some(lookup) = app_state.find_command(command_id) {
                recovered.push(SessionTab {
                    id: command_id.to_string(),
                    tab_type: TabType::Server,
                    project_id: lookup.project.id.clone(),
                    command_id: Some(command_id.to_string()),
                    pty_session_id: Some(command_id.to_string()),
                    label: Some(lookup.command.label.clone()),
                    ssh_connection_id: None,
                });
            }
        }

        app_state.merge_recovered_server_tabs(recovered)
    }

    pub fn restore_saved_server_tabs(
        &self,
        app_state: &mut AppState,
        dimensions: SessionDimensions,
    ) -> usize {
        let active_tab_id = app_state.active_tab_id.clone();
        let command_ids: Vec<String> = app_state
            .open_tabs
            .iter()
            .filter(|tab| matches!(tab.tab_type, TabType::Server))
            .filter_map(|tab| tab.command_id.clone())
            .collect();

        let mut restored = 0;
        for command_id in command_ids {
            let already_live = self
                .runtime_state()
                .sessions
                .get(&command_id)
                .map(|session| session.status.is_live())
                .unwrap_or(false);
            if already_live {
                continue;
            }
            if self
                .start_server(app_state, &command_id, dimensions)
                .is_ok()
            {
                restored += 1;
            }
        }

        app_state.active_tab_id = active_tab_id
            .filter(|tab_id| app_state.find_tab(tab_id).is_some())
            .or_else(|| app_state.open_tabs.first().map(|tab| tab.id.clone()));

        restored
    }

    fn session_exists(&self, session_id: &str) -> bool {
        let runtime_live = self
            .inner
            .runtime_state
            .read()
            .ok()
            .and_then(|runtime| {
                runtime
                    .sessions
                    .get(session_id)
                    .map(|session| session.status)
            })
            .map(SessionStatus::is_live)
            .unwrap_or(false);
        runtime_live
            && self
                .inner
                .sessions
                .lock()
                .ok()
                .map(|sessions| sessions.contains_key(session_id))
                .unwrap_or(false)
    }

    fn get_session(&self, session_id: &str) -> Result<Arc<TerminalSession>, String> {
        self.inner
            .sessions
            .lock()
            .map_err(|_| "Session store poisoned".to_string())?
            .get(session_id)
            .cloned()
            .ok_or_else(|| format!("Unknown session `{session_id}`"))
    }

    fn request_session_close(&self, session_id: &str, closed_by_user: bool) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.close(closed_by_user)
    }

    fn live_session_ids(&self) -> Vec<String> {
        self.runtime_state()
            .sessions
            .values()
            .filter(|session| session.status.is_live())
            .map(|session| session.session_id.clone())
            .collect()
    }

    fn live_session_pids(&self) -> Vec<u32> {
        self.runtime_state()
            .sessions
            .values()
            .filter(|session| session.status.is_live())
            .filter_map(|session| session.pid)
            .collect()
    }

    fn tracked_session_pids(&self, session_id: &str) -> Vec<u32> {
        let runtime = self.runtime_state();
        let mut pids = runtime
            .sessions
            .get(session_id)
            .map(|session| {
                let mut pids = session.resources.process_ids.clone();
                if let Some(pid) = session.pid {
                    pids.push(pid);
                }
                pids
            })
            .unwrap_or_default();
        pids.extend(pid_file::active_tracked_pids_for_session(session_id));
        pids.sort_unstable();
        pids.dedup();
        pids
    }

    fn wait_for_session_shutdown(&self, session_id: &str, timeout: Duration) -> bool {
        let started = Instant::now();
        loop {
            let session_live = self
                .runtime_state()
                .sessions
                .get(session_id)
                .map(|session| session.status.is_live())
                .unwrap_or(false);
            let tracked_pids = pid_file::active_tracked_pids_for_session(session_id);
            if !session_live && tracked_pids.is_empty() {
                return true;
            }
            if started.elapsed() >= timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn force_kill_session_processes(&self, session_id: &str) -> usize {
        let mut forced_kill_pids = 0;
        for pid in self.tracked_session_pids(session_id) {
            if !platform_service::is_pid_running(pid) {
                continue;
            }
            if platform_service::kill_process_tree(pid).is_ok()
                || !platform_service::is_pid_running(pid)
            {
                forced_kill_pids += 1;
            }
        }
        let _ = pid_file::prune_inactive_entries();
        forced_kill_pids
    }

    fn update_session_state(&self, session_id: &str, f: impl FnOnce(&mut SessionRuntimeState)) {
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            if let Some(session) = runtime.sessions.get_mut(session_id) {
                f(session);
            }
        }
    }

    fn forget_session(&self, session_id: &str) {
        if let Ok(mut sessions) = self.inner.sessions.lock() {
            sessions.remove(session_id);
        }
    }

    fn ensure_runtime_entry(&self, session_id: &str, cwd: PathBuf, dimensions: SessionDimensions) {
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            runtime
                .sessions
                .entry(session_id.to_string())
                .or_insert_with(|| {
                    SessionRuntimeState::new(
                        session_id.to_string(),
                        cwd,
                        dimensions,
                        self.inner.terminal_backend,
                    )
                });
        }
    }

    fn spawn_server_session(
        &self,
        launch: &ServerLaunchSpec,
        dimensions: SessionDimensions,
        activate_session: bool,
    ) -> Result<(), String> {
        let session_id = launch.command_id.clone();
        if activate_session {
            self.set_active_session(session_id.clone());
        }

        match spawn_server_session_with_inner(&self.inner, launch, dimensions) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.update_session_state(&session_id, |state| {
                    state.status = SessionStatus::Failed;
                    state.exit = Some(SessionExitState {
                        code: None,
                        signal: None,
                        closed_by_user: false,
                        summary: error.clone(),
                    });
                    state.mark_dirty();
                });
                Err(error)
            }
        }
    }

    fn restore_active_session(&self, active_session_id: Option<String>) {
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            runtime.active_session_id = active_session_id;
        }
    }

    fn spawn_ai_shell_session(
        &self,
        launch: &AiLaunchSpec,
        session_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<(), String> {
        self.set_active_session(session_id.to_string());

        if self.session_exists(session_id) {
            return Ok(());
        }

        let session = TerminalSession::spawn_command(
            session_id.to_string(),
            launch.cwd.clone(),
            dimensions,
            launch.shell_program.clone(),
            launch.shell_args.clone(),
            HashMap::new(),
            self.log_buffer_size(),
            None,
            self.inner.runtime_state.clone(),
            self.inner.debug_enabled,
        )
        .map_err(|error| {
            self.update_session_state(session_id, |state| {
                state.status = SessionStatus::Failed;
                state.exit = Some(SessionExitState {
                    code: None,
                    signal: None,
                    closed_by_user: false,
                    summary: error.clone(),
                });
                state.mark_dirty();
            });
            error
        })?;

        let session = Arc::new(session);
        self.inner
            .sessions
            .lock()
            .map_err(|_| "Session store poisoned".to_string())?
            .insert(session_id.to_string(), session.clone());

        let startup_command = launch.startup_command.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(AI_COMMAND_INJECTION_DELAY_MS));
            let _ = session.write_text(&(startup_command + "\r\n"));
        });

        Ok(())
    }

    fn spawn_ssh_session(
        &self,
        launch: &SshLaunchSpec,
        session_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<(), String> {
        self.set_active_session(session_id.to_string());

        if self.session_exists(session_id) {
            return Ok(());
        }

        let session = TerminalSession::spawn_command(
            session_id.to_string(),
            launch.cwd.clone(),
            dimensions,
            launch.program.clone(),
            launch.args.clone(),
            HashMap::new(),
            self.log_buffer_size(),
            None,
            self.inner.runtime_state.clone(),
            self.inner.debug_enabled,
        )
        .map_err(|error| {
            self.update_session_state(session_id, |state| {
                state.status = SessionStatus::Failed;
                state.exit = Some(SessionExitState {
                    code: None,
                    signal: None,
                    closed_by_user: false,
                    summary: error.clone(),
                });
                state.mark_dirty();
            });
            error
        })?;

        self.inner
            .sessions
            .lock()
            .map_err(|_| "Session store poisoned".to_string())?
            .insert(session_id.to_string(), Arc::new(session));

        Ok(())
    }
}

impl Drop for ProcessManagerInner {
    fn drop(&mut self) {
        self.background_stop.store(true, Ordering::SeqCst);
        if let Ok(mut handle) = self.background_thread.lock() {
            if let Some(handle) = handle.take() {
                let _ = handle.join();
            }
        }
        if let Ok(sessions) = self.sessions.lock() {
            for session in sessions.values() {
                let _ = session.close(false);
            }
        }
    }
}

fn debug_enabled() -> bool {
    std::env::var("DEVMANAGER_TERMINAL_DEBUG")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn spawn_background_tasks(inner: Arc<ProcessManagerInner>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut system = sysinfo::System::new();
        loop {
            if inner.background_stop.load(Ordering::SeqCst) {
                break;
            }

            refresh_resource_snapshots(&inner, &mut system);
            reconcile_ai_activity(&inner);
            handle_auto_restart(inner.clone());
            reconcile_exit_states(&inner);

            thread::sleep(Duration::from_secs(1));
        }
    })
}

fn refresh_resource_snapshots(inner: &ProcessManagerInner, system: &mut sysinfo::System) {
    let sessions: Vec<(String, u32)> = inner
        .runtime_state
        .read()
        .map(|runtime| {
            runtime
                .sessions
                .iter()
                .filter_map(|(id, session)| {
                    (session.status.is_live())
                        .then_some(session.pid.map(|pid| (id.clone(), pid)))
                        .flatten()
                })
                .collect()
        })
        .unwrap_or_default();

    if sessions.is_empty() {
        return;
    }

    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let tracked_processes: HashMap<String, pid_file::ManagedProcessRecord> =
        pid_file::tracked_processes()
            .into_iter()
            .map(|entry| (entry.session_id.clone(), entry))
            .collect();
    let sampled_at = Instant::now();
    let mut snapshots = Vec::with_capacity(sessions.len());

    for (session_id, pid) in sessions {
        let snapshot = tracked_processes
            .get(&session_id)
            .filter(|entry| entry.pid == pid)
            .filter(|entry| {
                platform_service::process_matches_identity_with_system(
                    system,
                    entry.pid,
                    entry.started_at_unix_secs,
                    entry.process_name.as_deref(),
                )
            })
            .and_then(|entry| {
                let root_pid = sysinfo::Pid::from_u32(entry.pid);
                let _root_process = system.process(root_pid)?;
                let process_tree_ids = collect_process_tree_ids(system, root_pid);
                let descendant_processes = process_tree_ids
                    .iter()
                    .skip(1)
                    .filter_map(|pid| {
                        platform_service::process_identity_with_system(system, pid.as_u32())
                    })
                    .collect::<Vec<_>>();
                let _ = pid_file::sync_session_descendant_processes(
                    session_id.as_str(),
                    entry.pid,
                    descendant_processes,
                );
                let mut cpu_percent = 0.0;
                let mut memory_bytes = 0;

                for tree_pid in &process_tree_ids {
                    if let Some(process) = system.process(*tree_pid) {
                        cpu_percent += process.cpu_usage();
                        memory_bytes += process.memory();
                    }
                }

                Some(ResourceSnapshot {
                    cpu_percent,
                    memory_bytes,
                    process_count: process_tree_ids.len() as u32,
                    process_ids: process_tree_ids
                        .into_iter()
                        .map(|pid| pid.as_u32())
                        .collect(),
                    last_sample_at: Some(sampled_at),
                })
            })
            .unwrap_or_default();
        snapshots.push((session_id, snapshot));
    }

    if let Ok(mut runtime) = inner.runtime_state.write() {
        for (session_id, snapshot) in snapshots {
            if let Some(session) = runtime.sessions.get_mut(&session_id) {
                session.note_resource_sample(snapshot);
            }
        }
    }
}

fn collect_process_tree_ids(system: &sysinfo::System, root_pid: sysinfo::Pid) -> Vec<sysinfo::Pid> {
    let mut process_ids = vec![root_pid];
    let mut cursor = 0;

    while cursor < process_ids.len() {
        let parent_pid = process_ids[cursor];
        cursor += 1;

        for (candidate_pid, process) in system.processes() {
            if process.parent() == Some(parent_pid) && !process_ids.contains(candidate_pid) {
                process_ids.push(*candidate_pid);
            }
        }
    }

    process_ids
}

fn reconcile_exit_states(inner: &ProcessManagerInner) {
    #[derive(Debug)]
    enum ExitReconciliation {
        RestoreInterruptedServer {
            session_id: String,
            cwd: PathBuf,
            dimensions: SessionDimensions,
        },
        MarkStopped {
            session_id: String,
        },
        MarkCrashed {
            session_id: String,
        },
    }

    let now = Instant::now();
    let mut actions = Vec::new();
    if let Ok(runtime) = inner.runtime_state.read() {
        for (id, session) in &runtime.sessions {
            if matches!(
                session.status,
                SessionStatus::Exited | SessionStatus::Failed
            ) && (session.command_id.is_some()
                || session.session_kind.is_ai()
                || matches!(session.session_kind, SessionKind::Ssh))
            {
                let closed_by_user = session
                    .exit
                    .as_ref()
                    .map(|exit| exit.closed_by_user)
                    .unwrap_or(false);
                let requested_stop = closed_by_user || session.has_recent_user_stop_request(now);
                if matches!(session.session_kind, SessionKind::Server)
                    && session.has_recent_user_interrupt(now)
                {
                    actions.push(ExitReconciliation::RestoreInterruptedServer {
                        session_id: id.clone(),
                        cwd: session.cwd.clone(),
                        dimensions: session.dimensions,
                    });
                } else if requested_stop {
                    actions.push(ExitReconciliation::MarkStopped {
                        session_id: id.clone(),
                    });
                } else {
                    actions.push(ExitReconciliation::MarkCrashed {
                        session_id: id.clone(),
                    });
                }
            }
        }
    }

    if actions.is_empty() {
        return;
    }

    for action in actions {
        match action {
            ExitReconciliation::RestoreInterruptedServer {
                session_id,
                cwd,
                dimensions,
            } => {
                if restore_interrupted_server_prompt(inner, &session_id, cwd, dimensions).is_err() {
                    if let Ok(mut runtime) = inner.runtime_state.write() {
                        if let Some(session) = runtime.sessions.get_mut(&session_id) {
                            session.status = SessionStatus::Stopped;
                            session.clear_user_exit_requests();
                            session.mark_dirty();
                        }
                    }
                }
            }
            ExitReconciliation::MarkStopped { session_id } => {
                if let Ok(mut runtime) = inner.runtime_state.write() {
                    if let Some(session) = runtime.sessions.get_mut(&session_id) {
                        session.status = SessionStatus::Stopped;
                        session.clear_user_exit_requests();
                        session.mark_dirty();
                    }
                }
            }
            ExitReconciliation::MarkCrashed { session_id } => {
                if let Ok(mut runtime) = inner.runtime_state.write() {
                    if let Some(session) = runtime.sessions.get_mut(&session_id) {
                        session.status = SessionStatus::Crashed;
                        session.clear_user_exit_requests();
                        session.mark_dirty();
                    }
                }
            }
        }
    }
}

fn reconcile_ai_activity(inner: &ProcessManagerInner) {
    let notification_sound = inner
        .notification_sound
        .read()
        .map(|sound| sound.clone())
        .unwrap_or(None);
    let mut should_notify = false;
    let now = Instant::now();

    if let Ok(mut runtime) = inner.runtime_state.write() {
        let active_session_id = runtime.active_session_id.clone();
        for (_session_id, session) in &mut runtime.sessions {
            session.reconcile_ai_idle(active_session_id.as_deref(), now);

            match session.check_pending_notification(now) {
                AiIdleTransition::BackgroundReady | AiIdleTransition::ForegroundReady => {
                    should_notify = true;
                }
                AiIdleTransition::NoChange => {}
            }
        }
    }

    if should_notify {
        notifications::play_notification_sound(notification_sound.as_deref());
    }
}

fn handle_auto_restart(inner: Arc<ProcessManagerInner>) {
    let mut restart_candidates = Vec::new();
    if let Ok(runtime) = inner.runtime_state.read() {
        for session in runtime.sessions.values() {
            if session.auto_restart
                && matches!(session.status, SessionStatus::Crashed)
                && session.server_launch.is_some()
            {
                restart_candidates.push(session.server_launch.clone().unwrap());
            }
        }
    }

    if restart_candidates.is_empty() {
        return;
    }

    for launch in restart_candidates {
        let delay = {
            let mut backoffs = inner
                .restart_backoffs
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let now = Instant::now();
            let entry = backoffs
                .entry(launch.command_id.clone())
                .or_insert(RestartBackoff {
                    delay: Duration::from_secs(1),
                    last_crash: now,
                });
            if now.duration_since(entry.last_crash) < Duration::from_secs(60) {
                entry.delay = std::cmp::min(entry.delay * 2, Duration::from_secs(30));
            } else {
                entry.delay = Duration::from_secs(1);
            }
            entry.last_crash = now;
            entry.delay
        };

        let launch_id = launch.command_id.clone();
        if let Ok(mut runtime) = inner.runtime_state.write() {
            if let Some(session) = runtime.sessions.get_mut(&launch_id) {
                session.status = SessionStatus::Starting;
                session.exit = Some(SessionExitState {
                    code: None,
                    signal: None,
                    closed_by_user: false,
                    summary: format!("Auto-restarting in {}s", delay.as_secs().max(1)),
                });
                session.mark_dirty();
            }
        }

        let launch_clone = launch.clone();
        let inner_clone = inner.clone();
        thread::spawn(move || {
            thread::sleep(delay);
            if inner_clone.background_stop.load(Ordering::SeqCst) {
                return;
            }
            if let Err(error) = spawn_server_session_with_inner(
                &inner_clone,
                &launch_clone,
                SessionDimensions::default(),
            ) {
                if let Ok(mut runtime) = inner_clone.runtime_state.write() {
                    if let Some(session) = runtime.sessions.get_mut(&launch_clone.command_id) {
                        session.status = SessionStatus::Failed;
                        session.exit = Some(SessionExitState {
                            code: None,
                            signal: None,
                            closed_by_user: false,
                            summary: format!("Auto-restart failed: {error}"),
                        });
                        session.mark_dirty();
                    }
                }
            }
        });
    }
}

fn build_command_env(folder: &ProjectFolder, command: &RunCommand) -> HashMap<String, String> {
    let mut env = HashMap::new();

    if let Some(env_file_path) = folder.env_file_path.as_deref() {
        let env_path = PathBuf::from(&folder.folder_path).join(env_file_path);
        if let Ok(file_env) = env_service::read_env_map(&env_path) {
            env.extend(file_env);
        }
    }

    if let Some(command_env) = command.env.as_ref() {
        for (key, value) in command_env {
            env.insert(key.clone(), value.clone());
        }
    }

    env
}

fn build_server_log_file_path(
    project: &Project,
    folder: &ProjectFolder,
    _command: &RunCommand,
) -> Option<PathBuf> {
    if project.save_log_files == Some(false) {
        return None;
    }

    let root = PathBuf::from(&project.root_path);
    if !root.is_dir() {
        return None;
    }

    let folder_name = Path::new(&folder.folder_path)
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "server".to_string());
    let slug = folder_name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    let file_name = if slug.is_empty() {
        "log-server.log".to_string()
    } else {
        format!("log-{slug}.log")
    };
    Some(root.join(file_name))
}

fn build_server_launch_command(settings: &Settings, command: &RunCommand) -> (String, Vec<String>) {
    if cfg!(target_os = "windows") {
        let mut args = vec!["/C".to_string(), command.command.clone()];
        args.extend(command.args.clone());
        return ("cmd".to_string(), args);
    }

    let shell = resolve_shell_path(settings);
    let args = if cfg!(target_os = "macos") {
        vec![
            "-l".to_string(),
            "-c".to_string(),
            build_shell_command_line(command),
        ]
    } else {
        vec![
            "-l".to_string(),
            "-c".to_string(),
            build_shell_command_line(command),
        ]
    };

    (shell, args)
}

fn build_ssh_launch_spec(
    app_state: &AppState,
    tab: &SessionTab,
    connection: &SSHConnection,
) -> SshLaunchSpec {
    let cwd = app_state
        .find_project(&tab.project_id)
        .map(|project| PathBuf::from(&project.root_path))
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));

    SshLaunchSpec {
        tab_id: tab.id.clone(),
        ssh_connection_id: connection.id.clone(),
        project_id: tab.project_id.clone(),
        cwd,
        program: "ssh".to_string(),
        args: vec![
            format!("{}@{}", connection.username.trim(), connection.host.trim()),
            "-p".to_string(),
            connection.port.to_string(),
        ],
    }
}

fn build_ai_launch_spec(
    settings: &Settings,
    project: &Project,
    tab: &SessionTab,
    session_id: &str,
) -> Result<AiLaunchSpec, String> {
    let cwd = PathBuf::from(&project.root_path);
    let cwd = if cwd.is_dir() {
        cwd
    } else {
        std::env::current_dir().unwrap_or_else(|_| ".".into())
    };
    let (shell_program, shell_args) = build_interactive_shell_command(settings);
    let startup_command = resolve_ai_startup_command(settings, tab.tab_type.clone())?;

    let launch = AiLaunchSpec {
        tab_id: tab.id.clone(),
        project_id: tab.project_id.clone(),
        tool: match tab.tab_type {
            TabType::Claude => SessionKind::Claude,
            TabType::Codex => SessionKind::Codex,
            _ => return Err(format!("Unsupported AI tab type `{}`", tab.id)),
        },
        cwd,
        shell_program,
        shell_args,
        startup_command,
    };

    if session_id.is_empty() {
        return Err("AI session id cannot be empty".to_string());
    }

    Ok(launch)
}

fn build_interactive_shell_command(settings: &Settings) -> (String, Vec<String>) {
    if cfg!(target_os = "windows") {
        return match settings.default_terminal.clone() {
            crate::models::DefaultTerminal::Powershell => {
                ("powershell.exe".to_string(), Vec::new())
            }
            crate::models::DefaultTerminal::Cmd => ("cmd.exe".to_string(), Vec::new()),
            crate::models::DefaultTerminal::Bash => (
                preferred_windows_bash_program(),
                vec!["--login".to_string()],
            ),
        };
    }

    match settings.default_terminal.clone() {
        crate::models::DefaultTerminal::Bash => ("bash".to_string(), Vec::new()),
        _ => {
            let shell = resolve_shell_path(settings);
            (shell, Vec::new())
        }
    }
}

fn interactive_shell_command_from_inner(inner: &ProcessManagerInner) -> (String, Vec<String>) {
    let settings = inner
        .settings
        .read()
        .map(|settings| settings.clone())
        .unwrap_or_default();
    build_interactive_shell_command(&settings)
}

fn resolve_ai_startup_command(settings: &Settings, tab_type: TabType) -> Result<String, String> {
    let configured = match tab_type {
        TabType::Claude => settings
            .claude_command
            .clone()
            .unwrap_or_else(|| DEFAULT_CLAUDE_COMMAND.to_string()),
        TabType::Codex => settings
            .codex_command
            .clone()
            .unwrap_or_else(|| DEFAULT_CODEX_COMMAND.to_string()),
        _ => return Err("Unsupported AI tab type".to_string()),
    };

    let trimmed = configured.trim().to_string();
    if trimmed.is_empty() {
        Err("AI command is empty".to_string())
    } else {
        Ok(trimmed)
    }
}

fn default_ai_label(tab_type: TabType) -> String {
    match tab_type {
        TabType::Claude => "Claude".to_string(),
        TabType::Codex => "Codex".to_string(),
        _ => "AI".to_string(),
    }
}

fn resolve_shell_path(settings: &Settings) -> String {
    if cfg!(target_os = "macos") {
        match settings.mac_terminal_profile {
            Some(crate::models::MacTerminalProfile::Zsh) => "/bin/zsh".to_string(),
            Some(crate::models::MacTerminalProfile::Bash) => "/bin/bash".to_string(),
            _ => std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string()),
        }
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
    }
}

fn build_shell_command_line(command: &RunCommand) -> String {
    let mut parts = Vec::with_capacity(command.args.len() + 1);
    parts.push(command.command.trim().to_string());
    for arg in &command.args {
        parts.push(shell_quote(arg));
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn spawn_server_session_with_inner(
    inner: &Arc<ProcessManagerInner>,
    launch: &ServerLaunchSpec,
    dimensions: SessionDimensions,
) -> Result<(), String> {
    let session_id = launch.command_id.clone();
    let session_live = inner
        .runtime_state
        .read()
        .ok()
        .and_then(|runtime| {
            runtime
                .sessions
                .get(&session_id)
                .map(|session| session.has_live_process())
        })
        .unwrap_or(false);
    let session_handle_exists = inner
        .sessions
        .lock()
        .ok()
        .map(|sessions| sessions.contains_key(&session_id))
        .unwrap_or(false);
    if session_live && session_handle_exists {
        return Ok(());
    }

    if let Ok(existing_session) = inner
        .sessions
        .lock()
        .map(|sessions| sessions.get(&session_id).cloned())
    {
        if let Some(session) = existing_session {
            return session.restart_command(
                launch.cwd.clone(),
                dimensions,
                launch.program.clone(),
                launch.args.clone(),
                launch.env.clone(),
                launch.log_file_path.clone(),
                true,
            );
        }
    }

    let session = TerminalSession::spawn_command(
        session_id.clone(),
        launch.cwd.clone(),
        dimensions,
        launch.program.clone(),
        launch.args.clone(),
        launch.env.clone(),
        inner
            .scrollback_lines
            .read()
            .map(|lines| *lines)
            .unwrap_or(10_000),
        launch.log_file_path.clone(),
        inner.runtime_state.clone(),
        inner.debug_enabled,
    )?;

    if let Ok(mut sessions) = inner.sessions.lock() {
        sessions.insert(session_id.clone(), Arc::new(session));
    }

    if let Ok(mut runtime) = inner.runtime_state.write() {
        if runtime.active_session_id.is_none() {
            runtime.active_session_id = Some(session_id);
        }
    }

    Ok(())
}

fn restore_interrupted_server_prompt(
    inner: &ProcessManagerInner,
    session_id: &str,
    cwd: PathBuf,
    dimensions: SessionDimensions,
) -> Result<(), String> {
    let (shell_program, shell_args) = interactive_shell_command_from_inner(inner);
    let existing_session = inner
        .sessions
        .lock()
        .map_err(|_| "Session store poisoned".to_string())?
        .get(session_id)
        .cloned();

    if let Some(session) = existing_session {
        session.restart_command(
            cwd.clone(),
            dimensions,
            shell_program.clone(),
            shell_args,
            HashMap::new(),
            None,
            false,
        )?;
    } else {
        let session = TerminalSession::spawn_command(
            session_id.to_string(),
            cwd.clone(),
            dimensions,
            shell_program.clone(),
            shell_args,
            HashMap::new(),
            inner
                .scrollback_lines
                .read()
                .map(|lines| *lines)
                .unwrap_or(10_000),
            None,
            inner.runtime_state.clone(),
            inner.debug_enabled,
        )?;
        inner
            .sessions
            .lock()
            .map_err(|_| "Session store poisoned".to_string())?
            .insert(session_id.to_string(), Arc::new(session));
    }

    if let Ok(mut runtime) = inner.runtime_state.write() {
        if let Some(session) = runtime.sessions.get_mut(session_id) {
            session.cwd = cwd;
            session.dimensions = dimensions;
            session.activate_interactive_shell(
                shell_program,
                "Server interrupted with Ctrl+C. Terminal ready.",
            );
        }
    }

    Ok(())
}

fn next_ai_session_id(tab_type: &TabType) -> String {
    let prefix = match tab_type {
        TabType::Claude => "claude",
        TabType::Codex => "codex",
        _ => "ai",
    };
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let counter = AI_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{millis:x}-{counter:x}")
}

fn next_ssh_session_id(connection_id: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let counter = SSH_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{connection_id}-{millis:x}-{counter:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AppConfig, Project, ProjectFolder, RunCommand, Settings};
    use crate::services::pid_file;
    use std::fs;
    use std::thread;

    #[test]
    fn clear_virtual_output_resets_terminal_snapshot() {
        let manager = ProcessManager::new();
        let cwd = temp_test_dir("clear-virtual-output");
        let session_id = "test-shell";

        manager
            .spawn_shell_session(session_id, &cwd, SessionDimensions::default(), None)
            .unwrap();
        manager
            .write_virtual_text(session_id, "hello world\r\n")
            .unwrap();

        let before = manager.session_view(session_id).expect("session view");
        assert!(screen_text(&before).contains("hello world"));

        manager.clear_virtual_output(session_id).unwrap();
        let after = manager.session_view(session_id).expect("session view");
        assert!(!screen_text(&after).contains("hello world"));

        let _ = manager.close_session(session_id);
    }

    #[test]
    fn restart_server_preserves_or_clears_logs_based_on_setting() {
        for clear_logs_on_restart in [false, true] {
            let manager = ProcessManager::new();
            let cwd = temp_test_dir(if clear_logs_on_restart {
                "restart-clear-logs"
            } else {
                "restart-preserve-logs"
            });
            let mut app_state = app_state_with_server(&cwd, clear_logs_on_restart);
            let command_id = "server-cmd";
            let dimensions = SessionDimensions::default();

            manager
                .start_server(&mut app_state, command_id, dimensions)
                .unwrap();
            wait_for_live_session(&manager, command_id);
            manager
                .write_virtual_text(command_id, "stale output\r\n")
                .unwrap();

            manager
                .restart_server(&mut app_state, command_id, dimensions)
                .unwrap();
            wait_for_live_session(&manager, command_id);

            let view = manager
                .session_view(command_id)
                .expect("server session view");
            let text = screen_text(&view);
            assert!(text.contains("Restarting"));
            if clear_logs_on_restart {
                assert!(!text.contains("stale output"));
            } else {
                assert!(text.contains("stale output"));
            }

            let _ = manager.stop_server(command_id);
        }
    }

    #[test]
    fn shutdown_managed_processes_prunes_tracked_processes() {
        let cwd = temp_test_dir("managed-shutdown");
        let pid_file_path = cwd.join("running-pids.json");
        let _pid_file_guard = pid_file::use_test_pid_file(pid_file_path);
        let manager = ProcessManager::new();
        let mut app_state = app_state_with_server(&cwd, true);
        let command_id = "server-cmd";
        let dimensions = SessionDimensions::default();

        manager
            .start_server(&mut app_state, command_id, dimensions)
            .unwrap();
        wait_for_live_session(&manager, command_id);
        wait_for_tracked_process(command_id);
        assert!(!pid_file::tracked_pids().is_empty());

        let report = manager.shutdown_managed_processes(Duration::from_secs(5));

        assert_eq!(report.requested_sessions, 1);
        assert_eq!(report.remaining_live_sessions, 0);
        assert_eq!(report.remaining_tracked_pids, 0);
        wait_for_tracked_processes_to_clear();
    }

    #[test]
    fn shell_sessions_are_tracked_in_managed_pid_ledger() {
        let cwd = temp_test_dir("managed-shell");
        let pid_file_path = cwd.join("running-pids.json");
        let _pid_file_guard = pid_file::use_test_pid_file(pid_file_path);
        let manager = ProcessManager::new();
        let session_id = "shell-session";

        manager
            .spawn_shell_session(session_id, &cwd, SessionDimensions::default(), None)
            .unwrap();
        wait_for_live_session(&manager, session_id);
        wait_for_tracked_process(session_id);

        let tracked = pid_file::tracked_processes();
        let shell_entry = tracked
            .iter()
            .find(|entry| entry.session_id == session_id)
            .expect("shell session was not tracked");
        assert_eq!(shell_entry.session_kind, "shell");
        assert!(pid_file::tracked_pids().contains(&shell_entry.pid));

        let _ = manager.close_session(session_id);
    }

    #[test]
    fn stopped_server_can_start_again_on_same_terminal_session() {
        let cwd = temp_test_dir("restart-after-stop");
        let pid_file_path = cwd.join("running-pids.json");
        let _pid_file_guard = pid_file::use_test_pid_file(pid_file_path);
        let manager = ProcessManager::new();
        let mut app_state = app_state_with_server(&cwd, true);
        let command_id = "server-cmd";
        let dimensions = SessionDimensions::default();

        manager
            .start_server(&mut app_state, command_id, dimensions)
            .unwrap();
        wait_for_running_session(&manager, command_id);

        assert!(manager.stop_server_and_wait(command_id, Duration::from_secs(5)));
        wait_for_stopped_session(&manager, command_id);

        manager
            .start_server(&mut app_state, command_id, dimensions)
            .unwrap();
        wait_for_running_session(&manager, command_id);
    }

    #[test]
    fn set_active_session_does_not_create_placeholder_runtime_entry() {
        let manager = ProcessManager::new();

        manager.set_active_session("missing-session");

        let runtime = manager.runtime_state();
        assert_eq!(
            runtime.active_session_id.as_deref(),
            Some("missing-session")
        );
        assert!(!runtime.sessions.contains_key("missing-session"));
    }

    fn temp_test_dir(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("devmanager-tests-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn app_state_with_server(cwd: &Path, clear_logs_on_restart: bool) -> AppState {
        let (command_text, args) = server_test_command();
        let command = RunCommand {
            id: "server-cmd".to_string(),
            label: "Server".to_string(),
            command: command_text,
            args,
            env: None,
            port: Some(43123),
            auto_restart: Some(false),
            clear_logs_on_restart: Some(clear_logs_on_restart),
        };
        let folder = ProjectFolder {
            id: "folder-1".to_string(),
            name: "Folder".to_string(),
            folder_path: cwd.to_string_lossy().to_string(),
            commands: vec![command],
            env_file_path: None,
            port_variable: None,
            hidden: Some(false),
        };
        let project = Project {
            id: "project-1".to_string(),
            name: "Project".to_string(),
            root_path: cwd.to_string_lossy().to_string(),
            folders: vec![folder],
            color: None,
            pinned: Some(false),
            notes: None,
            save_log_files: Some(false),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };

        AppState {
            config: AppConfig {
                version: crate::models::CURRENT_CONFIG_VERSION,
                projects: vec![project],
                settings: Settings::default(),
                ssh_connections: Vec::new(),
            },
            open_tabs: Vec::new(),
            active_tab_id: None,
            sidebar_collapsed: false,
            collapsed_projects: std::collections::BTreeSet::new(),
            window_bounds: None,
        }
    }

    fn wait_for_live_session(manager: &ProcessManager, session_id: &str) {
        for _ in 0..30 {
            if manager
                .runtime_state()
                .sessions
                .get(session_id)
                .map(|session| session.status.is_live())
                .unwrap_or(false)
            {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("session `{session_id}` never became live");
    }

    fn wait_for_running_session(manager: &ProcessManager, session_id: &str) {
        for _ in 0..30 {
            if manager
                .runtime_state()
                .sessions
                .get(session_id)
                .is_some_and(|session| {
                    session.status == SessionStatus::Running && session.pid.is_some()
                })
            {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("session `{session_id}` never became fully running");
    }

    fn wait_for_stopped_session(manager: &ProcessManager, session_id: &str) {
        for _ in 0..30 {
            if manager
                .runtime_state()
                .sessions
                .get(session_id)
                .is_some_and(|session| session.status == SessionStatus::Stopped)
            {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("session `{session_id}` never became stopped");
    }

    fn wait_for_tracked_process(session_id: &str) {
        for _ in 0..20 {
            if pid_file::tracked_processes()
                .into_iter()
                .any(|entry| entry.session_id == session_id)
            {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("session `{session_id}` was never tracked");
    }

    fn wait_for_tracked_processes_to_clear() {
        for _ in 0..20 {
            if pid_file::tracked_processes().is_empty() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("tracked process ledger never cleared");
    }

    fn screen_text(view: &TerminalSessionView) -> String {
        view.screen
            .lines
            .iter()
            .map(|line| {
                let mut text: String = line
                    .iter()
                    .map(|cell| {
                        if cell.character == '\u{00a0}' {
                            ' '
                        } else {
                            cell.character
                        }
                    })
                    .collect();
                while text.ends_with(' ') {
                    text.pop();
                }
                text
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[cfg(windows)]
    fn server_test_command() -> (String, Vec<String>) {
        (
            "ping".to_string(),
            vec!["127.0.0.1".to_string(), "-n".to_string(), "6".to_string()],
        )
    }

    #[cfg(not(windows))]
    fn server_test_command() -> (String, Vec<String>) {
        ("sleep".to_string(), vec!["5".to_string()])
    }
}
