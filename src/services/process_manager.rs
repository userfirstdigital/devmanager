use crate::models::{
    Project, ProjectFolder, RunCommand, SSHConnection, SessionTab, Settings, TabType,
};
use crate::notifications;
use crate::state::AppState;
use crate::state::{
    AiIdleTransition, AiLaunchSpec, ResourceSnapshot, RuntimeState, ServerLaunchSpec,
    SessionDimensions, SessionExitState, SessionKind, SessionRuntimeState, SessionStatus,
    SshLaunchSpec,
};
use crate::terminal::session::{TerminalBackend, TerminalSession, TerminalSessionView};
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
    terminal_backend: TerminalBackend,
    debug_enabled: bool,
    restart_backoffs: Mutex<HashMap<String, RestartBackoff>>,
    notification_sound: RwLock<Option<String>>,
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
            terminal_backend: TerminalBackend::PortablePtyFeedingAlacritty,
            debug_enabled,
            restart_backoffs: Mutex::new(HashMap::new()),
            notification_sound: RwLock::new(None),
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

    pub fn set_active_session(&self, session_id: impl Into<String>) {
        let session_id = session_id.into();
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            runtime.active_session_id = Some(session_id.clone());
            runtime
                .sessions
                .entry(session_id.clone())
                .or_insert_with(|| {
                    SessionRuntimeState::new(
                        session_id.clone(),
                        std::env::current_dir().unwrap_or_else(|_| ".".into()),
                        SessionDimensions::default(),
                        self.inner.terminal_backend,
                    )
                });
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

    pub fn paste_to_session(&self, session_id: &str, text: &str) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.paste_text(text)
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
        let session = self.get_session(session_id)?;
        session.close(true)
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
                .unwrap_or(false);
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
                .unwrap_or(false);
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
            if matches!(
                session.status,
                SessionStatus::Running | SessionStatus::Starting
            ) {
                app_state.open_server_tab(&project_id, &command_id, Some(command_label.clone()));
                self.set_active_session(session_id);
                return Ok(());
            }
        }

        self.set_active_session(session_id.clone());

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
        };

        app_state.open_server_tab(&project_id, &command_id, Some(command_label.clone()));

        self.update_session_state(&session_id, |state| {
            state.status = SessionStatus::Starting;
            state.cwd = cwd.clone();
            state.dimensions = dimensions;
            state.shell_program = program.clone();
            state.configure_server(launch_spec.clone());
            state.exit = None;
            state.mark_dirty();
        });

        self.spawn_server_session(&launch_spec, dimensions)?;

        self.update_session_state(&session_id, |state| {
            state.configure_server(launch_spec.clone());
        });

        Ok(())
    }

    pub fn stop_server(&self, command_id: &str) -> Result<(), String> {
        self.update_session_state(command_id, |state| {
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
        let started = Instant::now();
        while started.elapsed() < timeout {
            if let Some(session) = self.runtime_state().sessions.get(command_id) {
                if !session.status.is_live() {
                    return true;
                }
            } else {
                return true;
            }
            thread::sleep(Duration::from_millis(100));
        }
        self.update_session_state(command_id, |state| {
            if state.status.is_live() {
                state.status = SessionStatus::Stopped;
                state.pid = None;
                state.mark_dirty();
            }
        });
        false
    }

    pub fn restart_server(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<(), String> {
        let _ = self.stop_server_and_wait(command_id, Duration::from_secs(5));

        if let Ok(session) = self.get_session(command_id) {
            let _ = session.write_text("\r\n\x1b[33m--- Restarting... ---\x1b[0m\r\n");
        }

        self.start_server(app_state, command_id, dimensions)
    }

    pub fn start_all_for_project(
        &self,
        app_state: &mut AppState,
        project: &Project,
        dimensions: SessionDimensions,
    ) {
        for folder in &project.folders {
            for command in &folder.commands {
                let _ = self.start_server(app_state, &command.id, dimensions);
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
        let session_ids: Vec<String> = self
            .runtime_state()
            .sessions
            .values()
            .filter(|session| session.status.is_live())
            .map(|session| session.session_id.clone())
            .collect();

        for session_id in &session_ids {
            let _ = self.close_session(session_id);
        }

        session_ids.len()
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

    fn session_exists(&self, session_id: &str) -> bool {
        self.inner
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
    ) -> Result<(), String> {
        let session_id = launch.command_id.clone();
        self.set_active_session(session_id.clone());

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
                    if session.command_id.is_some()
                        && matches!(session.status, SessionStatus::Running)
                        && session.pid.is_some()
                    {
                        Some((id.clone(), session.pid.unwrap_or_default()))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    if sessions.is_empty() {
        return;
    }

    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    for (session_id, pid) in sessions {
        let pid = sysinfo::Pid::from(pid as usize);
        if let Some(process) = system.process(pid) {
            let snapshot = ResourceSnapshot {
                cpu_percent: process.cpu_usage(),
                memory_bytes: process.memory() * 1024,
                child_count: process.tasks().map(|tasks| tasks.len() as u32).unwrap_or(0),
                last_sample_at: Some(Instant::now()),
            };
            if let Ok(mut runtime) = inner.runtime_state.write() {
                if let Some(session) = runtime.sessions.get_mut(&session_id) {
                    session.note_resource_sample(snapshot);
                }
            }
        }
    }
}

fn reconcile_exit_states(inner: &ProcessManagerInner) {
    let mut to_crash = Vec::new();
    if let Ok(runtime) = inner.runtime_state.read() {
        for (id, session) in &runtime.sessions {
            if matches!(
                session.status,
                SessionStatus::Exited | SessionStatus::Failed
            ) && (session.command_id.is_some()
                || session.session_kind.is_ai()
                || matches!(session.session_kind, SessionKind::Ssh))
            {
                to_crash.push((id.clone(), session.exit.clone()));
            }
        }
    }

    if to_crash.is_empty() {
        return;
    }

    if let Ok(mut runtime) = inner.runtime_state.write() {
        for (session_id, exit) in to_crash {
            if let Some(session) = runtime.sessions.get_mut(&session_id) {
                let closed_by_user = exit
                    .as_ref()
                    .map(|exit| exit.closed_by_user)
                    .unwrap_or(false);
                if closed_by_user {
                    session.status = SessionStatus::Stopped;
                } else {
                    session.status = SessionStatus::Crashed;
                }
                session.mark_dirty();
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
    let mut transitions = Vec::new();
    let now = Instant::now();

    if let Ok(mut runtime) = inner.runtime_state.write() {
        let active_session_id = runtime.active_session_id.clone();
        for (session_id, session) in &mut runtime.sessions {
            match session.reconcile_ai_idle(active_session_id.as_deref(), now) {
                AiIdleTransition::NoChange => {}
                transition => transitions.push((session_id.clone(), transition)),
            }
        }
    }

    for (_, transition) in transitions {
        if matches!(
            transition,
            AiIdleTransition::BackgroundReady | AiIdleTransition::ForegroundReady
        ) {
            notifications::play_notification_sound(notification_sound.as_deref());
        }
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
        if let Ok(file_env) = read_env_file(&env_path) {
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

fn read_env_file(path: &Path) -> Result<HashMap<String, String>, String> {
    let contents = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut env = HashMap::new();

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim().to_string();
        let mut value = value.trim().to_string();
        if (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''))
        {
            value = value[1..value.len() - 1].to_string();
        }
        env.insert(key, value);
    }

    Ok(env)
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
                ("powershell".to_string(), vec!["-NoLogo".to_string()])
            }
            crate::models::DefaultTerminal::Cmd => ("cmd".to_string(), Vec::new()),
            crate::models::DefaultTerminal::Bash => ("cmd".to_string(), Vec::new()),
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
                .map(|session| session.status)
        })
        .map(SessionStatus::is_live)
        .unwrap_or(false);
    if session_live {
        return Ok(());
    }

    if let Ok(mut sessions) = inner.sessions.lock() {
        sessions.remove(&session_id);
    }

    let session = TerminalSession::spawn_command(
        session_id.clone(),
        launch.cwd.clone(),
        dimensions,
        launch.program.clone(),
        launch.args.clone(),
        launch.env.clone(),
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
