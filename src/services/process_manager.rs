use crate::ai::claude_hooks::{
    prepare_claude_launch_overlay, ClaudeHookRegistration, ClaudeHookRegistry,
    ClaudeHookRelayListener, ClaudeRegistryEvent, ClaudeShellKind,
};
use crate::models::{
    Project, ProjectFolder, RunCommand, SSHConnection, SessionTab, Settings, TabType,
};
use crate::notifications;
use crate::remote::presentation::{SemanticAdapterHealth, SemanticEventDraft, StableSessionKey};
use crate::remote::{ClaudeSemanticIdentity, RemoteActionResult};
use crate::services::process_ops::{
    next_op_id, ProcessOp, ProcessOpCompletion, ProcessOpContext, ProcessOpKind, ProcessOpQueue,
};
use crate::services::{env_service, pid_file, platform_service};
use crate::state::AppState;
use crate::state::{
    AiIdleTransition, AiLaunchSpec, ResourceSnapshot, RuntimeState, ServerLaunchSpec,
    SessionDimensions, SessionExitState, SessionKind, SessionRuntimeState, SessionStatus,
    SshLaunchSpec,
};
use crate::terminal::session::{
    bash_shell_args, preferred_windows_bash_program, TerminalBackend, TerminalSession,
    TerminalModeSnapshot, TerminalSessionView,
};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::Sender,
    Arc, Mutex, RwLock, Weak,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const AI_SESSION_ATTACH_GRACE_WINDOW: Duration = Duration::from_secs(30);

pub(crate) fn ai_session_needs_restore(
    session: Option<&SessionRuntimeState>,
    session_attached: bool,
    now: Instant,
) -> bool {
    let Some(session) = session else {
        return true;
    };

    if session.session_kind.is_ai() && !session_attached {
        if session.status == SessionStatus::Starting {
            return false;
        }
        if session.status == SessionStatus::Running
            && session.started_at.is_some_and(|started_at| {
                now.saturating_duration_since(started_at) <= AI_SESSION_ATTACH_GRACE_WINDOW
            })
        {
            return false;
        }
    }

    !session.status.is_live() || !session_attached
}

#[derive(Clone)]
pub struct ProcessManager {
    inner: Arc<ProcessManagerInner>,
    op_queue: Arc<ProcessOpQueue>,
    _claude_overlay_owner: Arc<ClaudeOverlayOwner>,
}

#[derive(Clone)]
pub enum RemoteSessionEvent {
    Output {
        session_id: String,
        bytes: Vec<u8>,
        mode: TerminalModeSnapshot,
    },
    Runtime {
        session_id: String,
        runtime: SessionRuntimeState,
    },
    Removed {
        session_id: String,
    },
    Semantic {
        draft: SemanticEventDraft,
    },
    ClaudeSemantic {
        identity: ClaudeSemanticIdentity,
        draft: SemanticEventDraft,
    },
    ClaudeAdapterRegistered {
        identity: ClaudeSemanticIdentity,
    },
    ClaudeAdapterRemoved {
        identity: ClaudeSemanticIdentity,
    },
    AdapterHealth {
        stable_session_key: StableSessionKey,
        health: SemanticAdapterHealth,
    },
}

type RemoteSessionEventHandler = Arc<dyn Fn(RemoteSessionEvent) + Send + Sync>;

pub(crate) struct ProcessManagerInner {
    sessions: Mutex<HashMap<String, Arc<TerminalSession>>>,
    runtime_state: Arc<RwLock<RuntimeState>>,
    runtime_revision: AtomicU64,
    observed_runtime_generations: Mutex<HashMap<String, u64>>,
    settings: RwLock<Settings>,
    terminal_backend: TerminalBackend,
    debug_enabled: bool,
    restart_backoffs: Mutex<HashMap<String, RestartBackoff>>,
    notification_sound: RwLock<Option<String>>,
    scrollback_lines: RwLock<usize>,
    remote_dirty_sessions: Arc<Mutex<BTreeSet<String>>>,
    remote_session_handler: RwLock<Option<RemoteSessionEventHandler>>,
    claude_hook_registry: Arc<ClaudeHookRegistry>,
    claude_hook_listener: Mutex<Option<ClaudeHookRelayListener>>,
    claude_hook_sessions: Mutex<HashMap<String, ClaudeHookSession>>,
    claude_hook_temp_root: PathBuf,
    claude_overlay_owner: Mutex<Weak<ClaudeOverlayOwner>>,
    background_stop: AtomicBool,
    background_thread: Mutex<Option<thread::JoinHandle<()>>>,
    op_queue: Mutex<Option<Arc<ProcessOpQueue>>>,
}

#[derive(Debug, Clone)]
struct ClaudeHookSession {
    registration: ClaudeHookRegistration,
    settings_path: PathBuf,
}

struct ClaudeOverlayOwner {
    inner: Weak<ProcessManagerInner>,
    process_root: PathBuf,
}

impl Drop for ClaudeOverlayOwner {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            drain_claude_hook_sessions_inner(&inner);
        }
        remove_owned_claude_overlay_root(&self.process_root);
    }
}

fn claude_semantic_identity(
    pty_session_id: &str,
    session: &ClaudeHookSession,
) -> ClaudeSemanticIdentity {
    ClaudeSemanticIdentity {
        pty_session_id: pty_session_id.to_string(),
        stable_session_key: session.registration.stable_session_key.clone(),
        registration_generation: session.registration.generation,
    }
}

fn claude_semantic_identity_for_registration(
    inner: &ProcessManagerInner,
    registration: &ClaudeHookRegistration,
) -> Option<ClaudeSemanticIdentity> {
    inner.claude_hook_sessions.lock().ok().and_then(|sessions| {
        sessions
            .iter()
            .find(|(_, session)| session.registration == *registration)
            .map(|(session_id, session)| claude_semantic_identity(session_id, session))
    })
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
static CLAUDE_OVERLAY_OWNER_COUNTER: AtomicU64 = AtomicU64::new(1);

const DEFAULT_CLAUDE_COMMAND: &str =
    "npx -y @anthropic-ai/claude-code@latest --dangerously-skip-permissions";
const DEFAULT_CODEX_COMMAND: &str =
    "npx -y @openai/codex@latest --dangerously-bypass-approvals-and-sandbox";
const AI_COMMAND_INJECTION_DELAY_MS: u64 = 500;
#[cfg(not(test))]
const SESSION_REAPER_TIMEOUT: Duration = Duration::from_secs(30);
/// Second force-kill retry window after the primary reaper timeout.
/// Same kill strategy as the first pass; gives stubborn descendants more time to die.
#[cfg(not(test))]
const SESSION_REAPER_ESCALATED_TIMEOUT: Duration = Duration::from_secs(30);

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessManager {
    pub fn new() -> Self {
        let debug_enabled = debug_enabled();
        let claude_hook_registry = Arc::new(ClaudeHookRegistry::default());
        let claude_hook_temp_root = prepare_claude_overlay_process_root();
        let inner = Arc::new(ProcessManagerInner {
            sessions: Mutex::new(HashMap::new()),
            runtime_state: Arc::new(RwLock::new(RuntimeState::new(debug_enabled))),
            runtime_revision: AtomicU64::new(1),
            observed_runtime_generations: Mutex::new(HashMap::new()),
            settings: RwLock::new(Settings::default()),
            terminal_backend: TerminalBackend::PortablePtyFeedingAlacritty,
            debug_enabled,
            restart_backoffs: Mutex::new(HashMap::new()),
            notification_sound: RwLock::new(None),
            scrollback_lines: RwLock::new(10_000),
            remote_dirty_sessions: Arc::new(Mutex::new(BTreeSet::new())),
            remote_session_handler: RwLock::new(None),
            claude_hook_registry: claude_hook_registry.clone(),
            claude_hook_listener: Mutex::new(None),
            claude_hook_sessions: Mutex::new(HashMap::new()),
            claude_hook_temp_root: claude_hook_temp_root.clone(),
            claude_overlay_owner: Mutex::new(Weak::new()),
            background_stop: AtomicBool::new(false),
            background_thread: Mutex::new(None),
            op_queue: Mutex::new(None),
        });
        let claude_overlay_owner = Arc::new(ClaudeOverlayOwner {
            inner: Arc::downgrade(&inner),
            process_root: claude_hook_temp_root,
        });
        if let Ok(mut owner) = inner.claude_overlay_owner.lock() {
            *owner = Arc::downgrade(&claude_overlay_owner);
        }

        let registry_inner = Arc::downgrade(&inner);
        claude_hook_registry.set_event_handler(Some(Arc::new(move |registration, event| {
            let Some(inner) = registry_inner.upgrade() else {
                return;
            };
            match event {
                ClaudeRegistryEvent::Semantic(draft) => {
                    let registry = inner.claude_hook_registry.clone();
                    registry.publish_if_current(&registration, || {
                        let identity =
                            claude_semantic_identity_for_registration(&inner, &registration);
                        if let Some(identity) = identity {
                            emit_remote_session_event(
                                &inner,
                                RemoteSessionEvent::ClaudeSemantic { identity, draft },
                            );
                        } else {
                            // Correlation is an optimization. If tracking was
                            // lost, preserve the provider event rather than
                            // hiding it behind an uncertain match.
                            emit_remote_session_event(
                                &inner,
                                RemoteSessionEvent::Semantic { draft },
                            );
                        }
                    });
                }
                ClaudeRegistryEvent::AdapterHealth {
                    stable_session_key,
                    health,
                } => {
                    let registry = inner.claude_hook_registry.clone();
                    registry.publish_if_current(&registration, || {
                        emit_remote_session_event(
                            &inner,
                            RemoteSessionEvent::AdapterHealth {
                                stable_session_key,
                                health,
                            },
                        );
                    });
                }
                ClaudeRegistryEvent::RegistrationDropped {
                    stable_session_key,
                    nonce,
                    generation,
                    was_latest,
                } => {
                    let removed_identities = {
                        let mut sessions = inner
                            .claude_hook_sessions
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let removed = sessions
                            .iter()
                            .filter(|(_, session)| {
                                session.registration.nonce == nonce
                                    && session.registration.generation == generation
                            })
                            .map(|(session_id, session)| {
                                claude_semantic_identity(session_id, session)
                            })
                            .collect::<Vec<_>>();
                        sessions.retain(|_, session| {
                            session.registration.nonce != nonce
                                || session.registration.generation != generation
                        });
                        removed
                    };
                    for identity in removed_identities {
                        emit_remote_session_event(
                            &inner,
                            RemoteSessionEvent::ClaudeAdapterRemoved { identity },
                        );
                    }
                    if was_latest {
                        let checked_key = stable_session_key.clone();
                        inner.claude_hook_registry.publish_if_not_superseded(
                            &checked_key,
                            generation,
                            || {
                                emit_remote_session_event(
                                    &inner,
                                    RemoteSessionEvent::AdapterHealth {
                                        stable_session_key,
                                        health: SemanticAdapterHealth::Degraded,
                                    },
                                );
                            },
                        );
                    }
                }
            }
        })));

        let op_queue = Arc::new(ProcessOpQueue::new(inner.clone()));
        if let Ok(mut slot) = inner.op_queue.lock() {
            *slot = Some(op_queue.clone());
        }

        let thread_handle = spawn_background_tasks(inner.clone());
        if let Ok(mut handle_slot) = inner.background_thread.lock() {
            *handle_slot = Some(thread_handle);
        }

        let op_queue = Arc::new(ProcessOpQueue::new(inner.clone()));

        Self {
            inner,
            op_queue,
            _claude_overlay_owner: claude_overlay_owner,
        }
    }

    pub fn drain_process_op_completions(&self) -> Vec<ProcessOpCompletion> {
        self.op_queue.drain_completions()
    }

    pub fn submit_process_op(&self, op: ProcessOp) -> Result<u64, String> {
        self.op_queue.submit(op)
    }

    fn schedule_start_server(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let Some(launch) =
            self.prepare_start_server(app_state, command_id, dimensions, activate_tab)?
        else {
            return Ok(());
        };
        let op_id = next_op_id();
        self.op_queue.submit(ProcessOp::StartServer {
            op_id,
            launch,
            dimensions,
            activate: activate_tab,
            response,
        })?;
        Ok(())
    }

    fn schedule_restart_server(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        banner: &str,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let (launch, clear_logs) =
            self.prepare_restart_server(app_state, command_id, dimensions, banner)?;
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::RestartServer {
                op_id,
                launch,
                dimensions,
                banner: banner.to_string(),
                clear_logs,
                response,
            })
            .map(|_| ())
    }

    fn schedule_stop_server_and_wait(
        &self,
        command_id: &str,
        wait: Duration,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::StopServer {
                op_id,
                command_id: command_id.to_string(),
                wait,
                response,
            })
            .map(|_| ())
    }

    fn enqueue_kill_port_op(
        &self,
        command_id: &str,
        port: u16,
        launch: ServerLaunchSpec,
        dimensions: SessionDimensions,
        banner: &str,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::KillPortAndRestart {
                op_id,
                command_id: command_id.to_string(),
                port,
                launch,
                dimensions,
                banner: banner.to_string(),
                response,
            })
            .map(|_| ())
    }

    fn schedule_stop_all_servers(
        &self,
        wait: Duration,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let command_ids: Vec<String> = self
            .runtime_state()
            .sessions
            .values()
            .filter(|session| session.command_id.is_some() && session.status.is_live())
            .filter_map(|session| session.command_id.clone())
            .collect();
        for command_id in &command_ids {
            self.update_session_state(command_id, |state| {
                state.note_user_stop_request();
                state.status = SessionStatus::Stopping;
                state.mark_dirty();
            });
        }
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::StopAll {
                op_id,
                command_ids,
                wait,
                response,
            })
            .map(|_| ())
    }

    pub fn schedule_shutdown(&self, timeout: Duration) -> Result<u64, String> {
        let op_id = next_op_id();
        self.op_queue.submit(ProcessOp::Shutdown { op_id, timeout })?;
        Ok(op_id)
    }

    pub fn enqueue_stop_server_and_wait(
        &self,
        command_id: &str,
        wait: Duration,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        self.schedule_stop_server_and_wait(command_id, wait, response)
    }

    pub fn enqueue_kill_process(
        &self,
        session_id: &str,
        pid: u32,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::KillProcess {
                op_id,
                session_id: session_id.to_string(),
                pid,
                response,
            })
            .map(|_| ())
    }

    pub fn enqueue_kill_process_tree(
        &self,
        session_id: &str,
        pid: u32,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::KillProcessTree {
                op_id,
                session_id: session_id.to_string(),
                pid,
                response,
            })
            .map(|_| ())
    }

    pub fn schedule_kill_port_and_restart(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        port: u16,
        dimensions: SessionDimensions,
        banner: &str,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let lookup = app_state
            .find_command(command_id)
            .ok_or_else(|| format!("Unknown command `{command_id}`"))?;
        let project_id = lookup.project.id.clone();
        let command_id_owned = lookup.command.id.clone();
        let command_label = lookup.command.label.clone();
        let command_auto_restart = lookup.command.auto_restart.unwrap_or(false);
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
            command_id: command_id_owned.clone(),
            project_id: project_id.clone(),
            cwd,
            program,
            args,
            env,
            auto_restart: command_auto_restart,
            log_file_path: build_server_log_file_path(
                lookup.project,
                lookup.folder,
                lookup.command,
            ),
        };
        app_state.open_server_tab(&project_id, &command_id_owned, Some(command_label));
        self.update_session_state(&command_id_owned, |state| {
            state.status = SessionStatus::Starting;
            state.mark_dirty();
        });
        self.enqueue_kill_port_op(
            &command_id_owned,
            port,
            launch_spec,
            dimensions,
            banner,
            response,
        )
    }

    fn prepare_start_server(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
    ) -> Result<Option<ServerLaunchSpec>, String> {
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
                return Ok(None);
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

        if !activate_tab {
            self.restore_active_session(previous_active_session_id);
        }

        Ok(Some(launch_spec))
    }

    fn prepare_restart_server(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        banner: &str,
    ) -> Result<(ServerLaunchSpec, bool), String> {
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

        self.update_session_state(&command_id, |state| {
            state.status = SessionStatus::Stopping;
            state.mark_dirty();
        });
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

        let _ = banner;
        Ok((launch_spec, clear_logs_on_restart))
    }

    fn schedule_spawn_ai(
        &self,
        launch: &AiLaunchSpec,
        session_id: &str,
        dimensions: SessionDimensions,
        activate: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let _ = activate;
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::SpawnAi {
                op_id,
                launch: launch.clone(),
                session_id: session_id.to_string(),
                dimensions,
                response,
            })
            .map(|_| ())
    }

    fn schedule_restart_ai(
        &self,
        close_session_id: Option<String>,
        launch: AiLaunchSpec,
        session_id: String,
        dimensions: SessionDimensions,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::RestartAi {
                op_id,
                close_session_id,
                launch,
                session_id: session_id.clone(),
                dimensions,
                response,
            })
            .map(|_| ())
    }

    fn schedule_close_ai(
        &self,
        session_id: &str,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::CloseAi {
                op_id,
                session_id: session_id.to_string(),
                response,
            })
            .map(|_| ())
    }

    fn schedule_start_ssh(
        &self,
        launch: SshLaunchSpec,
        session_id: String,
        dimensions: SessionDimensions,
        key_warning: Option<String>,
        activate: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let _ = activate;
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::StartSsh {
                op_id,
                launch,
                session_id: session_id.clone(),
                dimensions,
                key_warning,
                response,
            })
            .map(|_| ())
    }

    fn schedule_restart_ssh(
        &self,
        close_session_id: Option<String>,
        launch: SshLaunchSpec,
        session_id: String,
        dimensions: SessionDimensions,
        key_warning: Option<String>,
        activate: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let _ = activate;
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::RestartSsh {
                op_id,
                close_session_id,
                launch,
                session_id: session_id.clone(),
                dimensions,
                key_warning,
                response,
            })
            .map(|_| ())
    }

    fn schedule_close_ssh(
        &self,
        session_id: Option<String>,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let op_id = next_op_id();
        self.op_queue
            .submit(ProcessOp::CloseSsh {
                op_id,
                session_id,
                response,
            })
            .map(|_| ())
    }

    pub fn runtime_state(&self) -> RuntimeState {
        self.inner
            .runtime_state
            .read()
            .map(|runtime| runtime.clone())
            .unwrap_or_default()
    }

    pub fn runtime_revision(&self) -> u64 {
        self.inner.runtime_revision.load(Ordering::Relaxed)
    }

    pub fn register_runtime_session(&self, session: SessionRuntimeState) {
        let session_id = session.session_id.clone();
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            runtime.sessions.insert(session_id.clone(), session);
        }
        bump_runtime_revision(&self.inner);
        emit_tracked_remote_runtime_snapshot(&self.inner, &session_id);
    }

    pub fn terminal_backend(&self) -> TerminalBackend {
        self.inner.terminal_backend
    }

    pub fn drain_remote_dirty_sessions(&self) -> Vec<String> {
        let Ok(mut dirty) = self.inner.remote_dirty_sessions.lock() else {
            return Vec::new();
        };
        let values = dirty.iter().cloned().collect();
        dirty.clear();
        values
    }

    pub fn debug_enabled(&self) -> bool {
        self.inner.debug_enabled
    }

    pub fn set_remote_session_handler(&self, handler: Option<RemoteSessionEventHandler>) {
        if let Ok(mut slot) = self.inner.remote_session_handler.write() {
            *slot = handler;
        }
    }

    fn claude_hook_endpoint(&self) -> Result<String, String> {
        let mut listener = self
            .inner
            .claude_hook_listener
            .lock()
            .map_err(|_| "Claude hook listener lock is poisoned".to_string())?;
        if listener.is_none() {
            *listener = Some(ClaudeHookRelayListener::start(
                self.inner.claude_hook_registry.clone(),
            )?);
        }
        listener
            .as_ref()
            .map(|listener| listener.endpoint().to_string())
            .ok_or_else(|| "Claude hook listener did not start".to_string())
    }

    fn prepare_claude_launch_for_session(
        &self,
        launch: &mut AiLaunchSpec,
        session_id: &str,
        temp_root: &Path,
    ) {
        if launch.tool != SessionKind::Claude {
            return;
        }
        let stable_session_key = StableSessionKey::from_tab(&launch.tab_id);
        let endpoint = match self.claude_hook_endpoint() {
            Ok(endpoint) => endpoint,
            Err(_) => {
                emit_remote_session_event(
                    &self.inner,
                    RemoteSessionEvent::AdapterHealth {
                        stable_session_key,
                        health: SemanticAdapterHealth::Degraded,
                    },
                );
                return;
            }
        };
        let executable = match std::env::current_exe() {
            Ok(executable) => executable,
            Err(_) => {
                emit_remote_session_event(
                    &self.inner,
                    RemoteSessionEvent::AdapterHealth {
                        stable_session_key,
                        health: SemanticAdapterHealth::Degraded,
                    },
                );
                return;
            }
        };
        let overlay = prepare_claude_launch_overlay(
            &self.inner.claude_hook_registry,
            stable_session_key.clone(),
            &launch.startup_command,
            claude_shell_kind(&launch.shell_program),
            &executable,
            &endpoint,
            temp_root,
            Instant::now(),
        );
        let health = overlay.health;
        if let (Some(registration), Some(settings_path)) =
            (overlay.registration, overlay.settings_path)
        {
            let session = ClaudeHookSession {
                registration,
                settings_path,
            };
            let identity = claude_semantic_identity(session_id, &session);
            let previous = self
                .inner
                .claude_hook_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(session_id.to_string(), session);
            launch.startup_command = overlay.startup_command;
            emit_remote_session_event(
                &self.inner,
                RemoteSessionEvent::ClaudeAdapterRegistered { identity },
            );
            if let Some(previous) = previous {
                emit_remote_session_event(
                    &self.inner,
                    RemoteSessionEvent::ClaudeAdapterRemoved {
                        identity: claude_semantic_identity(session_id, &previous),
                    },
                );
                self.inner
                    .claude_hook_registry
                    .unregister_registration(&previous.registration);
            }
        }
        emit_remote_session_event(
            &self.inner,
            RemoteSessionEvent::AdapterHealth {
                stable_session_key,
                health,
            },
        );
    }

    fn cleanup_claude_hook_session(&self, session_id: &str) {
        let registration = self
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
        if let Some(registration) = registration {
            emit_remote_session_event(
                &self.inner,
                RemoteSessionEvent::ClaudeAdapterRemoved {
                    identity: claude_semantic_identity(session_id, &registration),
                },
            );
            self.inner
                .claude_hook_registry
                .unregister_registration(&registration.registration);
            let _ = std::fs::remove_file(registration.settings_path);
        }
    }

    pub fn drain_claude_hook_adapter(&self) {
        drain_claude_hook_sessions_inner(&self.inner);
        remove_owned_claude_overlay_root(&self.inner.claude_hook_temp_root);
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
        let mut cleared_unseen_ready = false;
        let mut active_changed = false;
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            active_changed = runtime.active_session_id.as_deref() != Some(session_id.as_str());
            if active_changed {
                runtime.active_session_id = Some(session_id.clone());
            }
            if let Some(session) = runtime.sessions.get_mut(&session_id) {
                cleared_unseen_ready = session.unseen_ready;
                session.clear_unseen_ready();
            }
        }
        if active_changed || cleared_unseen_ready {
            bump_runtime_revision(&self.inner);
        }
        if cleared_unseen_ready {
            mark_remote_session_dirty(&self.inner, &session_id);
            emit_tracked_remote_runtime_snapshot(&self.inner, &session_id);
        }
    }

    pub fn spawn_shell_session(
        &self,
        session_id: impl Into<String>,
        cwd: &Path,
        dimensions: SessionDimensions,
        default_terminal: Option<crate::models::DefaultTerminal>,
        mac_terminal_profile: Option<crate::models::MacTerminalProfile>,
    ) -> Result<(), String> {
        let session_id = session_id.into();
        self.set_active_session(session_id.clone());

        if self.session_exists(&session_id) {
            return Ok(());
        }

        let _ = force_reap_session_processes_until_clear(
            &self.inner,
            &session_id,
            Duration::from_secs(2),
        );

        match TerminalSession::spawn(
            session_id.clone(),
            cwd.to_path_buf(),
            dimensions,
            default_terminal,
            mac_terminal_profile,
            self.inner
                .settings
                .read()
                .map(|settings| settings.shell_integration_enabled)
                .unwrap_or(true),
            self.log_buffer_size(),
            self.inner.runtime_state.clone(),
            self.inner.debug_enabled,
            Some(session_change_notifier(
                self.inner.clone(),
                session_id.clone(),
            )),
            Some(session_output_notifier(
                self.inner.clone(),
                session_id.clone(),
            )),
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

    pub fn scroll_session_to_offset(
        &self,
        session_id: &str,
        display_offset: usize,
    ) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.scroll_to_display_offset(display_offset)
    }

    pub fn scroll_session_to_buffer_line(
        &self,
        session_id: &str,
        buffer_line: usize,
    ) -> Result<(), String> {
        let session = self.get_session(session_id)?;
        session.scroll_to_buffer_line(buffer_line)
    }

    pub fn session_screen_text(&self, session_id: &str) -> Result<String, String> {
        let session = self.get_session(session_id)?;
        Ok(session.screen_text())
    }

    pub fn session_scrollback_text(&self, session_id: &str) -> Result<String, String> {
        let session = self.get_session(session_id)?;
        Ok(session.scrollback_text())
    }

    pub fn session_replay_bytes(&self, session_id: &str) -> Result<Vec<u8>, String> {
        let session = self.get_session(session_id)?;
        Ok(session.replay_bytes())
    }

    pub fn search_session(
        &self,
        session_id: &str,
        query: &str,
        case_sensitive: bool,
        max_results: usize,
    ) -> Result<Vec<crate::terminal::session::TerminalSearchMatch>, String> {
        let session = self.get_session(session_id)?;
        Ok(session.search(query, case_sensitive, max_results))
    }

    pub fn close_session(&self, session_id: &str) -> Result<(), String> {
        self.request_session_close(session_id, true)
    }

    pub fn close_tab(&self, app_state: &mut AppState, tab_id: &str) -> Result<(), String> {
        let Some(tab) = app_state.find_tab(tab_id).cloned() else {
            return Ok(());
        };

        match tab.tab_type {
            TabType::Server => {
                let command_id = tab.command_id.unwrap_or_else(|| tab.id.clone());
                let _ = self.enqueue_stop_server_and_wait(&command_id, Duration::ZERO, None);
                app_state.remove_tab(tab_id);
            }
            TabType::Claude | TabType::Codex => {
                self.close_ai_session(app_state, tab_id)?;
            }
            TabType::Ssh => {
                self.close_ssh_session(app_state, tab_id)?;
                app_state.remove_tab(tab_id);
            }
        }

        Ok(())
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

    pub fn session_view_from_runtime(
        &self,
        runtime: &RuntimeState,
        session_id: &str,
    ) -> Option<TerminalSessionView> {
        let runtime_session = runtime.sessions.get(session_id)?.clone();
        let session = self.get_session(session_id).ok()?;

        Some(TerminalSessionView {
            runtime: runtime_session,
            screen: session.snapshot(),
        })
    }

    pub fn session_view(&self, session_id: &str) -> Option<TerminalSessionView> {
        let runtime = self.runtime_state();
        self.session_view_from_runtime(&runtime, session_id)
    }

    pub fn all_session_views(&self) -> HashMap<String, TerminalSessionView> {
        let runtime = self.runtime_state();
        let mut views = HashMap::new();
        for (session_id, runtime_session) in runtime.sessions.iter() {
            if let Ok(session) = self.get_session(session_id) {
                views.insert(
                    session_id.clone(),
                    TerminalSessionView {
                        runtime: runtime_session.clone(),
                        screen: session.snapshot(),
                    },
                );
            }
        }
        views
    }

    pub fn record_frame(&self, session_id: &str, render_duration: Duration) {
        let render_micros = render_duration.as_micros() as u64;
        match self.inner.runtime_state.try_write() {
            Ok(mut runtime) => {
                if let Some(session) = runtime.sessions.get_mut(session_id) {
                    session.record_frame(render_micros);
                }
            }
            Err(std::sync::TryLockError::Poisoned(error)) => {
                let mut runtime = error.into_inner();
                if let Some(session) = runtime.sessions.get_mut(session_id) {
                    session.record_frame(render_micros);
                }
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
    }

    pub fn start_ai_session(
        &self,
        app_state: &mut AppState,
        project_id: &str,
        tab_type: TabType,
        dimensions: SessionDimensions,
    ) -> Result<String, String> {
        self.start_ai_session_activate(app_state, project_id, tab_type, dimensions, true)
    }

    /// Same as `start_ai_session` but lets the caller decide whether to
    /// force the new tab to become the native UI's active tab. Remote
    /// clients should pass `activate = false` so a browser launching a new
    /// AI session doesn't yank the desktop window's focus onto a
    /// mid-spawn terminal — that path triggers a heavy GPUI render of a
    /// PTY being flooded with Claude Code's boot banner and stalls the
    /// main thread badly enough for Windows to mark the window
    /// "(Not Responding)".
    pub fn start_ai_session_activate(
        &self,
        app_state: &mut AppState,
        project_id: &str,
        tab_type: TabType,
        dimensions: SessionDimensions,
        activate: bool,
    ) -> Result<String, String> {
        self.start_ai_session_activate_with_response(
            app_state,
            project_id,
            tab_type,
            dimensions,
            activate,
            None,
        )
    }

    pub fn start_ai_session_activate_with_response(
        &self,
        app_state: &mut AppState,
        project_id: &str,
        tab_type: TabType,
        dimensions: SessionDimensions,
        activate: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<String, String> {
        if app_state.find_project(project_id).is_none() {
            return Err(format!("Unknown project `{project_id}`"));
        }
        let label = app_state.next_ai_label(project_id, tab_type.clone());
        let session_id = next_ai_session_id(&tab_type);
        let tab_id = session_id.clone();

        app_state.open_ai_tab_with_activation(
            project_id,
            tab_type,
            tab_id.clone(),
            session_id,
            Some(label),
            activate,
        );

        self.ensure_ai_session_for_tab_with_response(
            app_state,
            &tab_id,
            dimensions,
            activate,
            false,
            response,
        )
    }

    pub fn ensure_ai_session_for_tab(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        force_new_session: bool,
    ) -> Result<String, String> {
        self.ensure_ai_session_for_tab_with_response(
            app_state,
            tab_id,
            dimensions,
            activate_tab,
            force_new_session,
            None,
        )
    }

    pub fn ensure_ai_session_for_tab_with_response(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        force_new_session: bool,
        response: Option<Sender<RemoteActionResult>>,
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
            let existing_runtime = self
                .runtime_state()
                .sessions
                .get(existing_session_id)
                .cloned();
            let session_attached = self.get_session(existing_session_id).is_ok();
            if !force_new_session
                && !ai_session_needs_restore(
                    existing_runtime.as_ref(),
                    session_attached,
                    Instant::now(),
                )
            {
                if activate_tab {
                    let _ = app_state.select_tab(&tab.id);
                    if session_attached {
                        self.set_active_session(existing_session_id.to_string());
                    }
                }
                return Ok(existing_session_id.to_string());
            }
            self.forget_session(existing_session_id);
        }

        let session_id = next_ai_session_id(&tab.tab_type);
        let mut launch =
            build_ai_launch_spec(&app_state.config.settings, &project, &tab, &session_id)?;
        self.prepare_claude_launch_for_session(
            &mut launch,
            &session_id,
            &self.inner.claude_hook_temp_root,
        );

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

        if let Err(error) =
            self.schedule_spawn_ai(&launch, &session_id, dimensions, activate_tab, response)
        {
            self.cleanup_claude_hook_session(&session_id);
            return Err(error);
        }
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
        self.restart_ai_session_activate(app_state, tab_id, dimensions, true)
    }

    /// Same as `restart_ai_session` but lets the caller keep the native UI's
    /// current tab/session active. Remote-triggered AI restarts use this to
    /// recycle the PTY without yanking the desktop window onto the restarted
    /// terminal.
    pub fn restart_ai_session_activate(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
    ) -> Result<String, String> {
        self.restart_ai_session_activate_with_response(
            app_state,
            tab_id,
            dimensions,
            activate_tab,
            None,
        )
    }

    pub fn restart_ai_session_activate_with_response(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<String, String> {
        let existing_session_id = app_state
            .find_ai_tab(tab_id)
            .and_then(|tab| tab.pty_session_id.clone());

        let tab = app_state
            .find_ai_tab(tab_id)
            .cloned()
            .ok_or_else(|| format!("Unknown AI tab `{tab_id}`"))?;
        let project = app_state
            .find_project(&tab.project_id)
            .cloned()
            .ok_or_else(|| format!("Unknown project `{}`", tab.project_id))?;

        let session_id = next_ai_session_id(&tab.tab_type);
        let mut launch =
            build_ai_launch_spec(&app_state.config.settings, &project, &tab, &session_id)?;
        self.prepare_claude_launch_for_session(
            &mut launch,
            &session_id,
            &self.inner.claude_hook_temp_root,
        );

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

        if let Err(error) = self.schedule_restart_ai(
            existing_session_id,
            launch,
            session_id.clone(),
            dimensions,
            response,
        ) {
            self.cleanup_claude_hook_session(&session_id);
            return Err(error);
        }
        if activate_tab {
            self.set_active_session(session_id.clone());
        }
        Ok(session_id)
    }

    pub fn close_ai_session(&self, app_state: &mut AppState, tab_id: &str) -> Result<(), String> {
        self.close_ai_session_with_response(app_state, tab_id, None)
    }

    pub fn close_ai_session_with_response(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let session_id = app_state
            .find_ai_tab(tab_id)
            .and_then(|tab| tab.pty_session_id.clone());

        app_state.remove_tab(tab_id);
        if let Some(session_id) = session_id {
            self.schedule_close_ai(&session_id, response)?;
        }
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

        let next_active = active_tab_id
            .filter(|tab_id| app_state.find_tab(tab_id).is_some())
            .or_else(|| app_state.open_tabs.first().map(|tab| tab.id.clone()));
        if app_state.active_tab_id != next_active {
            app_state.active_tab_id = next_active;
            app_state.mark_dirty();
        }

        report
    }

    pub fn start_ssh_session(
        &self,
        app_state: &mut AppState,
        connection_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<String, String> {
        self.start_ssh_session_with_response(app_state, connection_id, dimensions, None)
    }

    pub fn start_ssh_session_with_response(
        &self,
        app_state: &mut AppState,
        connection_id: &str,
        dimensions: SessionDimensions,
        response: Option<Sender<RemoteActionResult>>,
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

        self.ensure_ssh_session_for_tab_with_response(
            app_state,
            &tab_id,
            dimensions,
            true,
            false,
            response,
        )
    }

    pub fn ensure_ssh_session_for_tab(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        force_new_session: bool,
    ) -> Result<String, String> {
        self.ensure_ssh_session_for_tab_with_response(
            app_state,
            tab_id,
            dimensions,
            activate_tab,
            force_new_session,
            None,
        )
    }

    pub fn ensure_ssh_session_for_tab_with_response(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        force_new_session: bool,
        response: Option<Sender<RemoteActionResult>>,
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
        let (key_file, key_error) = match self.materialize_ssh_key(&connection) {
            Ok(path) => (path, None),
            Err(error) => (None, Some(error)),
        };
        let launch = build_ssh_launch_spec(app_state, &tab, &connection, key_file.as_deref());

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

        self.schedule_start_ssh(
            launch,
            session_id.clone(),
            dimensions,
            key_error,
            activate_tab,
            response,
        )?;
        Ok(session_id)
    }

    pub fn restart_ssh_session(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<String, String> {
        self.restart_ssh_session_with_response(app_state, tab_id, dimensions, None)
    }

    pub fn restart_ssh_session_with_response(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        dimensions: SessionDimensions,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<String, String> {
        let existing_session_id = app_state
            .find_ssh_tab(tab_id)
            .and_then(|tab| tab.pty_session_id.clone());

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

        let session_id = next_ssh_session_id(&connection_id);
        let (key_file, key_error) = match self.materialize_ssh_key(&connection) {
            Ok(path) => (path, None),
            Err(error) => (None, Some(error)),
        };
        let launch = build_ssh_launch_spec(app_state, &tab, &connection, key_file.as_deref());

        let _ = app_state.update_ssh_tab_session(&tab.id, Some(session_id.clone()));
        let _ = app_state.select_tab(&tab.id);

        self.ensure_runtime_entry(&session_id, launch.cwd.clone(), dimensions);
        self.update_session_state(&session_id, |state| {
            state.status = SessionStatus::Starting;
            state.cwd = launch.cwd.clone();
            state.dimensions = dimensions;
            state.shell_program = launch.program.clone();
            state.configure_ssh(launch.clone());
            state.exit = None;
        });

        self.schedule_restart_ssh(
            existing_session_id,
            launch,
            session_id.clone(),
            dimensions,
            key_error,
            true,
            response,
        )?;
        Ok(session_id)
    }

    pub fn close_ssh_session(&self, app_state: &mut AppState, tab_id: &str) -> Result<(), String> {
        self.close_ssh_session_with_response(app_state, tab_id, None)
    }

    pub fn close_ssh_session_with_response(
        &self,
        app_state: &mut AppState,
        tab_id: &str,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        let session_id = app_state
            .find_ssh_tab(tab_id)
            .and_then(|tab| tab.pty_session_id.clone());

        let _ = app_state.update_ssh_tab_session(tab_id, None);
        self.schedule_close_ssh(session_id, response)
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
        let next_active = active_tab_id
            .filter(|tab_id| app_state.find_tab(tab_id).is_some())
            .or_else(|| app_state.open_tabs.first().map(|tab| tab.id.clone()));
        if app_state.active_tab_id != next_active {
            app_state.active_tab_id = next_active;
            app_state.mark_dirty();
        }

        report
    }

    pub fn start_server(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<(), String> {
        self.schedule_start_server(app_state, command_id, dimensions, true, None)
    }

    pub fn start_server_in_background(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
    ) -> Result<(), String> {
        self.schedule_start_server(app_state, command_id, dimensions, false, None)
    }

    pub fn start_server_with_remote_response(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        activate_tab: bool,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        self.schedule_start_server(
            app_state,
            command_id,
            dimensions,
            activate_tab,
            response,
        )
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
        if remaining_tracked_pids.is_empty() {
            self.update_session_state(command_id, |state| {
                state.status = SessionStatus::Stopped;
                state.pid = None;
                state.resources = ResourceSnapshot::default();
                state.exit = Some(SessionExitState {
                    code: None,
                    signal: None,
                    closed_by_user: true,
                    summary: "Managed process did not stop cleanly.".to_string(),
                });
                state.mark_dirty();
            });
        } else {
            self.update_session_state(command_id, |state| {
                state.status = SessionStatus::Failed;
                state.pid = None;
                state.resources = ResourceSnapshot {
                    process_count: remaining_tracked_pids.len() as u32,
                    process_ids: remaining_tracked_pids.clone(),
                    last_sample_at: Some(Instant::now()),
                    ..ResourceSnapshot::default()
                };
                state.reap_incomplete = true;
                state.exit = Some(SessionExitState {
                    code: None,
                    signal: None,
                    closed_by_user: true,
                    summary: format!(
                        "Managed process left {} tracked child process(es) running.",
                        remaining_tracked_pids.len()
                    ),
                });
                state.mark_dirty();
            });
        }
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
        self.schedule_restart_server(app_state, command_id, dimensions, banner, None)
    }

    pub fn restart_server_with_remote_response(
        &self,
        app_state: &mut AppState,
        command_id: &str,
        dimensions: SessionDimensions,
        banner: &str,
        response: Option<Sender<RemoteActionResult>>,
    ) -> Result<(), String> {
        self.schedule_restart_server(app_state, command_id, dimensions, banner, response)
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
        let command_ids: Vec<String> = self
            .runtime_state()
            .sessions
            .values()
            .filter(|session| {
                session.project_id.as_deref() == Some(project_id)
                    && session.command_id.is_some()
                    && matches!(
                        session.status,
                        SessionStatus::Running | SessionStatus::Starting
                    )
            })
            .filter_map(|session| session.command_id.clone())
            .collect();
        for command_id in &command_ids {
            self.update_session_state(command_id, |state| {
                state.note_user_stop_request();
                state.status = SessionStatus::Stopping;
                state.mark_dirty();
            });
            let _ = self.enqueue_stop_server_and_wait(command_id, Duration::ZERO, None);
        }
    }

    pub fn stop_all_servers(&self) -> usize {
        let count = self
            .runtime_state()
            .sessions
            .values()
            .filter(|session| session.command_id.is_some() && session.status.is_live())
            .count();
        let _ = self.schedule_stop_all_servers(Duration::from_secs(5), None);
        count
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
        let op_id = match self.schedule_shutdown(timeout) {
            Ok(op_id) => op_id,
            Err(_) => {
                return ManagedShutdownReport {
                    requested_sessions: self.live_session_count(),
                    ..ManagedShutdownReport::default()
                };
            }
        };
        let started = Instant::now();
        loop {
            for completion in self.drain_process_op_completions() {
                if completion.op_id == op_id {
                    if let Some(report) = completion.context.shutdown_report {
                        return report;
                    }
                    return ManagedShutdownReport::default();
                }
            }
            if started.elapsed() >= timeout + Duration::from_secs(2) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        ManagedShutdownReport {
            requested_sessions: self.live_session_count(),
            remaining_live_sessions: self.live_session_count(),
            remaining_tracked_pids: pid_file::active_tracked_pids().len(),
            ..ManagedShutdownReport::default()
        }
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

        let next_active = active_tab_id
            .filter(|tab_id| app_state.find_tab(tab_id).is_some())
            .or_else(|| app_state.open_tabs.first().map(|tab| tab.id.clone()));
        if app_state.active_tab_id != next_active {
            app_state.active_tab_id = next_active;
            app_state.mark_dirty();
        }

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

    pub fn session_attached(&self, session_id: &str) -> bool {
        self.inner
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
        let result = match self.get_session(session_id) {
            Ok(session) => session.close(closed_by_user),
            Err(error) => {
                self.cleanup_claude_hook_session(session_id);
                self.note_missing_session_close_request(session_id, closed_by_user);
                Err(error)
            }
        };
        self.spawn_session_reaper(session_id.to_string());
        result
    }

    fn note_missing_session_close_request(&self, session_id: &str, closed_by_user: bool) {
        self.update_session_state(session_id, |session| {
            if session.status.is_live() {
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
        });
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
        force_reap_session_processes(&self.inner, session_id)
    }

    fn spawn_session_reaper(&self, session_id: String) {
        #[cfg(test)]
        {
            let _ =
                self.reap_session_processes_until_clear(&session_id, Duration::from_millis(100));
            if !pid_file::active_tracked_pids_for_session(&session_id).is_empty()
                || live_runtime_root_running(&self.inner, &session_id)
            {
                self.note_reap_incomplete(&session_id);
            } else {
                mark_session_reaped(&self.inner, &session_id);
            }
        }

        #[cfg(not(test))]
        {
            let manager = self.clone();
            thread::spawn(move || {
                let _ =
                    manager.reap_session_processes_until_clear(&session_id, SESSION_REAPER_TIMEOUT);
                if pid_file::active_tracked_pids_for_session(&session_id).is_empty()
                    && !live_runtime_root_running(&manager.inner, &session_id)
                {
                    return;
                }
                let _ = manager.reap_session_processes_until_clear(
                    &session_id,
                    SESSION_REAPER_ESCALATED_TIMEOUT,
                );
                if !pid_file::active_tracked_pids_for_session(&session_id).is_empty()
                    || live_runtime_root_running(&manager.inner, &session_id)
                {
                    manager.note_reap_incomplete(&session_id);
                }
            });
        }
    }

    fn note_reap_incomplete(&self, session_id: &str) {
        let remaining_tracked = pid_file::active_tracked_processes_for_session(session_id);
        let mut remaining_pids = BTreeSet::new();
        let mut processes = Vec::new();

        for entry in &remaining_tracked {
            let root_verified = platform_service::process_matches_identity(
                entry.pid,
                entry.started_at_unix_secs,
                entry.process_name.as_deref(),
            );
            if root_verified {
                remaining_pids.insert(entry.pid);
                let root_name = entry
                    .process_name
                    .clone()
                    .unwrap_or_else(|| format!("pid-{}", entry.pid));
                processes.push(crate::state::ProcessResourceNode {
                    pid: entry.pid,
                    parent_pid: None,
                    name: root_name,
                    cpu_percent: 0.0,
                    memory_bytes: 0,
                });
                for descendant in platform_service::collect_descendant_process_identities(entry.pid)
                {
                    if remaining_pids.insert(descendant.pid) {
                        processes.push(crate::state::ProcessResourceNode {
                            pid: descendant.pid,
                            parent_pid: Some(entry.pid),
                            name: descendant
                                .process_name
                                .clone()
                                .unwrap_or_else(|| format!("pid-{}", descendant.pid)),
                            cpu_percent: 0.0,
                            memory_bytes: 0,
                        });
                    }
                }
            } else {
                for descendant in &entry.descendant_processes {
                    if platform_service::process_matches_identity(
                        descendant.pid,
                        descendant.started_at_unix_secs,
                        descendant.process_name.as_deref(),
                    ) && remaining_pids.insert(descendant.pid)
                    {
                        processes.push(crate::state::ProcessResourceNode {
                            pid: descendant.pid,
                            parent_pid: Some(entry.pid),
                            name: descendant
                                .process_name
                                .clone()
                                .unwrap_or_else(|| format!("pid-{}", descendant.pid)),
                            cpu_percent: 0.0,
                            memory_bytes: 0,
                        });
                    }
                }
            }
        }

        if let Some(root_pid) = live_runtime_root_pid(&self.inner, session_id) {
            if platform_service::is_pid_running(root_pid) && remaining_pids.insert(root_pid) {
                let name = platform_service::capture_process_identity(root_pid)
                    .and_then(|identity| identity.process_name)
                    .unwrap_or_else(|| format!("pid-{root_pid}"));
                processes.push(crate::state::ProcessResourceNode {
                    pid: root_pid,
                    parent_pid: None,
                    name,
                    cpu_percent: 0.0,
                    memory_bytes: 0,
                });
            }
            for descendant in platform_service::collect_descendant_process_identities(root_pid) {
                if remaining_pids.insert(descendant.pid) {
                    processes.push(crate::state::ProcessResourceNode {
                        pid: descendant.pid,
                        parent_pid: Some(root_pid),
                        name: descendant
                            .process_name
                            .clone()
                            .unwrap_or_else(|| format!("pid-{}", descendant.pid)),
                        cpu_percent: 0.0,
                        memory_bytes: 0,
                    });
                }
            }
        }

        if remaining_pids.is_empty() {
            // Nothing verified remains — finish the stop instead of leaving Stopping forever.
            mark_session_reaped(&self.inner, session_id);
            return;
        }

        let remaining_pids: Vec<u32> = remaining_pids.into_iter().collect();
        self.update_session_state(session_id, |state| {
            state.reap_incomplete = true;
            state.status = SessionStatus::Failed;
            state.pid = None;
            state.resources = ResourceSnapshot {
                process_count: remaining_pids.len() as u32,
                process_ids: remaining_pids.clone(),
                processes: processes.clone(),
                last_sample_at: Some(Instant::now()),
                ..ResourceSnapshot::default()
            };
            let summary = format!(
                "Session close left {} tracked process(es) running.",
                remaining_pids.len()
            );
            state.exit = Some(SessionExitState {
                code: None,
                signal: None,
                closed_by_user: state
                    .exit
                    .as_ref()
                    .map(|exit| exit.closed_by_user)
                    .unwrap_or(true),
                summary,
            });
            state.mark_dirty();
        });
    }

    fn reap_session_processes_until_clear(&self, session_id: &str, timeout: Duration) -> usize {
        let reaped = force_reap_session_processes_until_clear(&self.inner, session_id, timeout);
        if pid_file::active_tracked_pids_for_session(session_id).is_empty()
            && !live_runtime_root_running(&self.inner, session_id)
        {
            mark_session_reaped(&self.inner, session_id);
        }
        reaped
    }

    fn update_session_state(&self, session_id: &str, f: impl FnOnce(&mut SessionRuntimeState)) {
        let mut runtime_changed = false;
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            if let Some(session) = runtime.sessions.get_mut(session_id) {
                let dirty_before = session.dirty_generation;
                f(session);
                runtime_changed = session.dirty_generation != dirty_before;
            }
        }
        if runtime_changed {
            bump_runtime_revision(&self.inner);
            mark_remote_session_dirty(&self.inner, session_id);
            emit_tracked_remote_runtime_snapshot(&self.inner, session_id);
        }
    }

    fn forget_session(&self, session_id: &str) {
        self.cleanup_claude_hook_session(session_id);
        if let Ok(mut sessions) = self.inner.sessions.lock() {
            sessions.remove(session_id);
        }
        mark_remote_session_dirty(&self.inner, session_id);
        emit_remote_session_removed(&self.inner, session_id);
    }

    fn ensure_runtime_entry(&self, session_id: &str, cwd: PathBuf, dimensions: SessionDimensions) {
        let mut inserted = false;
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            runtime
                .sessions
                .entry(session_id.to_string())
                .or_insert_with(|| {
                    inserted = true;
                    SessionRuntimeState::new(
                        session_id.to_string(),
                        cwd,
                        dimensions,
                        self.inner.terminal_backend,
                    )
                });
        }
        if inserted {
            bump_runtime_revision(&self.inner);
            mark_remote_session_dirty(&self.inner, session_id);
            emit_tracked_remote_runtime_snapshot(&self.inner, session_id);
        }
    }

    fn restore_active_session(&self, active_session_id: Option<String>) {
        let mut changed = false;
        if let Ok(mut runtime) = self.inner.runtime_state.write() {
            if runtime.active_session_id != active_session_id {
                runtime.active_session_id = active_session_id;
                changed = true;
            }
        }
        if changed {
            bump_runtime_revision(&self.inner);
        }
    }

    fn materialize_ssh_key(&self, connection: &SSHConnection) -> Result<Option<PathBuf>, String> {
        let dir = crate::persistence::app_config_dir()
            .map_err(|error| format!("resolve config dir: {error}"))?
            .join("ssh-keys");
        materialize_ssh_key_in(&dir, connection)
    }

    /// Best-effort cleanup when a connection is deleted or its key cleared.
    /// Materialized files are permission-locked, so a missed delete is low risk.
    pub fn remove_materialized_ssh_key(connection_id: &str) {
        let Ok(dir) = crate::persistence::app_config_dir() else {
            return;
        };
        let _ = std::fs::remove_file(
            dir.join("ssh-keys").join(safe_key_file_name(connection_id)),
        );
    }
}

fn drain_claude_hook_sessions_inner(inner: &ProcessManagerInner) {
    let sessions = {
        let mut sessions = inner
            .claude_hook_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *sessions)
    };
    for (session_id, session) in sessions {
        emit_remote_session_event(
            inner,
            RemoteSessionEvent::ClaudeAdapterRemoved {
                identity: claude_semantic_identity(&session_id, &session),
            },
        );
        inner
            .claude_hook_registry
            .unregister_registration(&session.registration);
        let _ = std::fs::remove_file(session.settings_path);
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
        if let Ok(mut slot) = self.op_queue.lock() {
            if let Some(queue) = slot.take() {
                queue.shutdown();
            }
        }
        if let Ok(sessions) = self.sessions.lock() {
            for session in sessions.values() {
                let _ = session.close(false);
            }
        }
        drain_claude_hook_sessions_inner(self);
        remove_owned_claude_overlay_root(&self.claude_hook_temp_root);
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
    let sessions: Vec<(String, u32, bool)> = inner
        .runtime_state
        .read()
        .map(|runtime| {
            runtime
                .sessions
                .iter()
                .filter_map(|(id, session)| {
                    if session.status.is_live() {
                        return session
                            .pid
                            .map(|pid| (id.clone(), pid, session.session_kind.is_ai()));
                    }
                    if session.reap_incomplete {
                        let ledger_pid = pid_file::active_tracked_processes_for_session(id)
                            .into_iter()
                            .next()
                            .map(|entry| entry.pid);
                        let pid = ledger_pid.or_else(|| session.resources.process_ids.first().copied());
                        return pid.map(|pid| (id.clone(), pid, false));
                    }
                    None
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

    for (session_id, pid, is_ai_session) in sessions {
        let (snapshot, awaiting_external_editor) = tracked_processes
            .get(&session_id)
            .filter(|entry| {
                entry.pid == pid
                    || entry
                        .descendant_processes
                        .iter()
                        .any(|descendant| descendant.pid == pid)
            })
            .and_then(|entry| {
                let sample_root = if platform_service::process_matches_identity_with_system(
                    system,
                    entry.pid,
                    entry.started_at_unix_secs,
                    entry.process_name.as_deref(),
                ) {
                    entry.pid
                } else if entry.pid == pid {
                    return None;
                } else {
                    pid
                };
                let root_pid = sysinfo::Pid::from_u32(sample_root);
                let _root_process = system.process(root_pid)?;
                let process_tree_ids = collect_process_tree_ids(system, root_pid);
                let descendant_processes = process_tree_ids
                    .iter()
                    .skip(1)
                    .filter_map(|tree_pid| {
                        platform_service::process_identity_with_system(system, tree_pid.as_u32())
                    })
                    .collect::<Vec<_>>();
                let awaiting_external_editor =
                    is_ai_session && is_blocking_external_editor(&descendant_processes);
                if sample_root == entry.pid {
                    let _ = pid_file::sync_session_descendant_processes(
                        session_id.as_str(),
                        entry.pid,
                        descendant_processes,
                    );
                }
                let mut cpu_percent = 0.0;
                let mut memory_bytes = 0;
                let mut processes = Vec::with_capacity(process_tree_ids.len());

                for tree_pid in &process_tree_ids {
                    if let Some(process) = system.process(*tree_pid) {
                        let process_cpu = process.cpu_usage();
                        let process_memory = process.memory();
                        cpu_percent += process_cpu;
                        memory_bytes += process_memory;
                        let name = platform_service::process_identity_with_system(
                            system,
                            tree_pid.as_u32(),
                        )
                        .and_then(|identity| identity.process_name)
                        .unwrap_or_else(|| format!("pid-{}", tree_pid.as_u32()));
                        processes.push(crate::state::ProcessResourceNode {
                            pid: tree_pid.as_u32(),
                            parent_pid: process.parent().map(|parent| parent.as_u32()),
                            name,
                            cpu_percent: process_cpu,
                            memory_bytes: process_memory,
                        });
                    }
                }

                Some((
                    ResourceSnapshot {
                        cpu_percent,
                        memory_bytes,
                        process_count: processes.len() as u32,
                        process_ids: processes.iter().map(|process| process.pid).collect(),
                        processes,
                        last_sample_at: Some(sampled_at),
                    },
                    awaiting_external_editor,
                ))
            })
            .or_else(|| {
                // Live runtime root without a matching ledger entry yet.
                let root_pid = sysinfo::Pid::from_u32(pid);
                let process = system.process(root_pid)?;
                let process_tree_ids = collect_process_tree_ids(system, root_pid);
                let mut cpu_percent = 0.0;
                let mut memory_bytes = 0;
                let mut processes = Vec::with_capacity(process_tree_ids.len());
                for tree_pid in &process_tree_ids {
                    if let Some(tree_process) = system.process(*tree_pid) {
                        let process_cpu = tree_process.cpu_usage();
                        let process_memory = tree_process.memory();
                        cpu_percent += process_cpu;
                        memory_bytes += process_memory;
                        let name = platform_service::process_identity_with_system(
                            system,
                            tree_pid.as_u32(),
                        )
                        .and_then(|identity| identity.process_name)
                        .unwrap_or_else(|| format!("pid-{}", tree_pid.as_u32()));
                        processes.push(crate::state::ProcessResourceNode {
                            pid: tree_pid.as_u32(),
                            parent_pid: tree_process.parent().map(|parent| parent.as_u32()),
                            name,
                            cpu_percent: process_cpu,
                            memory_bytes: process_memory,
                        });
                    }
                }
                let _ = process;
                Some((
                    ResourceSnapshot {
                        cpu_percent,
                        memory_bytes,
                        process_count: processes.len() as u32,
                        process_ids: processes.iter().map(|node| node.pid).collect(),
                        processes,
                        last_sample_at: Some(sampled_at),
                    },
                    false,
                ))
            })
            .unwrap_or_default();
        snapshots.push((session_id, snapshot, awaiting_external_editor));
    }

    let mut touched_sessions = Vec::new();
    let mut cleared_reap_sessions = Vec::new();
    if let Ok(mut runtime) = inner.runtime_state.write() {
        for (session_id, snapshot, awaiting_external_editor) in snapshots {
            if let Some(session) = runtime.sessions.get_mut(&session_id) {
                let dirty_before = session.dirty_generation;
                let cleared_unreaped = session.reap_incomplete && snapshot.process_ids.is_empty();
                session.note_resource_sample(snapshot);
                session.note_external_editor_wait(awaiting_external_editor);
                if cleared_unreaped {
                    session.reap_incomplete = false;
                    session.status = SessionStatus::Stopped;
                    session.pid = None;
                    session.resources = ResourceSnapshot::default();
                    session.mark_dirty();
                    cleared_reap_sessions.push(session_id.clone());
                }
                if session.dirty_generation != dirty_before {
                    touched_sessions.push(session_id);
                }
            }
        }
    }
    if !touched_sessions.is_empty() {
        bump_runtime_revision(inner);
    }
    for session_id in touched_sessions {
        emit_tracked_remote_runtime_snapshot(inner, &session_id);
    }
    for session_id in cleared_reap_sessions {
        let _ = pid_file::prune_inactive_entries();
        emit_tracked_remote_runtime_snapshot(inner, &session_id);
    }
}

fn is_blocking_external_editor(descendants: &[platform_service::ProcessIdentity]) -> bool {
    descendants.iter().any(|identity| {
        identity
            .process_name
            .as_deref()
            .map(normalize_process_name_for_detection)
            .is_some_and(|name| {
                matches!(
                    name.as_str(),
                    "code"
                        | "code-insiders"
                        | "cursor"
                        | "windsurf"
                        | "notepad"
                        | "notepad++"
                        | "sublime_text"
                        | "devenv"
                        | "gvim"
                        | "nvim-qt"
                )
            })
    })
}

fn normalize_process_name_for_detection(name: &str) -> String {
    name.trim().trim_end_matches(".exe").to_ascii_lowercase()
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

fn force_reap_session_processes_until_clear(
    inner: &Arc<ProcessManagerInner>,
    session_id: &str,
    timeout: Duration,
) -> usize {
    let started_at = Instant::now();
    let mut reaped = 0;
    loop {
        reaped += force_reap_session_processes(inner, session_id);
        if pid_file::active_tracked_pids_for_session(session_id).is_empty()
            && !live_runtime_root_running(inner, session_id)
        {
            break;
        }
        if started_at.elapsed() >= timeout {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    reaped
}

fn force_reap_session_processes(inner: &Arc<ProcessManagerInner>, session_id: &str) -> usize {
    let mut forced_kill_pids = 0;
    for pid in collect_session_reap_pids(inner, session_id) {
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

fn collect_session_reap_pids(inner: &Arc<ProcessManagerInner>, session_id: &str) -> Vec<u32> {
    let mut pids = BTreeSet::new();

    for entry in pid_file::active_tracked_processes_for_session(session_id) {
        let root_verified = platform_service::process_matches_identity(
            entry.pid,
            entry.started_at_unix_secs,
            entry.process_name.as_deref(),
        );
        if root_verified {
            pids.insert(entry.pid);
            for descendant in platform_service::collect_descendant_process_identities(entry.pid) {
                pids.insert(descendant.pid);
            }
        }
        for descendant in entry.descendant_processes {
            if platform_service::process_matches_identity(
                descendant.pid,
                descendant.started_at_unix_secs,
                descendant.process_name.as_deref(),
            ) {
                pids.insert(descendant.pid);
            }
        }
    }

    if let Some(root_pid) = live_runtime_root_pid(inner, session_id) {
        if platform_service::is_pid_running(root_pid) {
            pids.insert(root_pid);
            for descendant in platform_service::collect_descendant_process_identities(root_pid) {
                pids.insert(descendant.pid);
            }
        }
    }

    pids.into_iter().collect()
}

fn live_runtime_root_pid(inner: &Arc<ProcessManagerInner>, session_id: &str) -> Option<u32> {
    inner.runtime_state.read().ok().and_then(|runtime| {
        runtime
            .sessions
            .get(session_id)
            .and_then(|session| (session.status.is_live()).then_some(session.pid).flatten())
    })
}

fn live_runtime_root_running(inner: &Arc<ProcessManagerInner>, session_id: &str) -> bool {
    live_runtime_root_pid(inner, session_id).is_some_and(platform_service::is_pid_running)
}

fn mark_session_reaped(inner: &Arc<ProcessManagerInner>, session_id: &str) {
    let mut changed = false;
    if let Ok(mut runtime) = inner.runtime_state.write() {
        if let Some(session) = runtime.sessions.get_mut(session_id) {
            if session.status.is_live() || session.reap_incomplete {
                let dirty_before = session.dirty_generation;
                session.status = SessionStatus::Stopped;
                session.pid = None;
                session.resources = ResourceSnapshot::default();
                session.reap_incomplete = false;
                if session.exit.is_none() {
                    session.exit = Some(SessionExitState {
                        code: None,
                        signal: None,
                        closed_by_user: true,
                        summary: "Session processes cleared.".to_string(),
                    });
                }
                session.mark_dirty();
                changed = session.dirty_generation != dirty_before;
            }
        }
    }
    if changed {
        bump_runtime_revision(inner);
        emit_tracked_remote_runtime_snapshot(inner, session_id);
    }
}

fn reconcile_exit_states(inner: &Arc<ProcessManagerInner>) {
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
                let _ = force_reap_session_processes(inner, &session_id);
                if restore_interrupted_server_prompt(inner, &session_id, cwd, dimensions).is_err() {
                    let mut changed = false;
                    if let Ok(mut runtime) = inner.runtime_state.write() {
                        if let Some(session) = runtime.sessions.get_mut(&session_id) {
                            let dirty_before = session.dirty_generation;
                            session.status = SessionStatus::Stopped;
                            session.clear_user_exit_requests();
                            session.mark_dirty();
                            changed = session.dirty_generation != dirty_before;
                        }
                    }
                    if changed {
                        bump_runtime_revision(inner);
                        emit_tracked_remote_runtime_snapshot(inner, &session_id);
                    }
                }
            }
            ExitReconciliation::MarkStopped { session_id } => {
                let _ = force_reap_session_processes(inner, &session_id);
                let mut changed = false;
                if let Ok(mut runtime) = inner.runtime_state.write() {
                    if let Some(session) = runtime.sessions.get_mut(&session_id) {
                        let dirty_before = session.dirty_generation;
                        session.status = SessionStatus::Stopped;
                        session.clear_user_exit_requests();
                        session.mark_dirty();
                        changed = session.dirty_generation != dirty_before;
                    }
                }
                if changed {
                    bump_runtime_revision(inner);
                    emit_tracked_remote_runtime_snapshot(inner, &session_id);
                }
            }
            ExitReconciliation::MarkCrashed { session_id } => {
                let _ = force_reap_session_processes(inner, &session_id);
                let mut changed = false;
                if let Ok(mut runtime) = inner.runtime_state.write() {
                    if let Some(session) = runtime.sessions.get_mut(&session_id) {
                        let dirty_before = session.dirty_generation;
                        session.status = SessionStatus::Crashed;
                        session.clear_user_exit_requests();
                        session.mark_dirty();
                        changed = session.dirty_generation != dirty_before;
                    }
                }
                if changed {
                    bump_runtime_revision(inner);
                    emit_tracked_remote_runtime_snapshot(inner, &session_id);
                }
            }
        }
    }
}

fn reconcile_ai_activity(inner: &Arc<ProcessManagerInner>) {
    let notification_sound = inner
        .notification_sound
        .read()
        .map(|sound| sound.clone())
        .unwrap_or(None);
    let mut should_notify = false;
    let now = Instant::now();

    if let Ok(mut runtime) = inner.runtime_state.write() {
        let active_session_id = runtime.active_session_id.clone();
        let mut touched_sessions = Vec::new();
        for (session_id, session) in &mut runtime.sessions {
            let gen_before = session.dirty_generation;
            session.reconcile_ai_idle(active_session_id.as_deref(), now);
            let mut changed = session.dirty_generation != gen_before;

            match session.check_pending_notification(now) {
                AiIdleTransition::BackgroundReady | AiIdleTransition::ForegroundReady => {
                    should_notify = true;
                    session.notification_count += 1;
                    changed = true;
                }
                AiIdleTransition::NoChange => {}
            }

            if changed {
                touched_sessions.push(session_id.clone());
            }
        }
        drop(runtime);
        if !touched_sessions.is_empty() {
            bump_runtime_revision(inner);
        }
        for session_id in touched_sessions {
            emit_tracked_remote_runtime_snapshot(inner, &session_id);
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
        let mut changed = false;
        if let Ok(mut runtime) = inner.runtime_state.write() {
            if let Some(session) = runtime.sessions.get_mut(&launch_id) {
                let dirty_before = session.dirty_generation;
                session.status = SessionStatus::Starting;
                session.exit = Some(SessionExitState {
                    code: None,
                    signal: None,
                    closed_by_user: false,
                    summary: format!("Auto-restarting in {}s", delay.as_secs().max(1)),
                });
                session.mark_dirty();
                changed = session.dirty_generation != dirty_before;
            }
        }
        if changed {
            bump_runtime_revision(&inner);
            emit_tracked_remote_runtime_snapshot(&inner, &launch_id);
        }

        let launch_clone = launch.clone();
        let inner_clone = inner.clone();
        thread::spawn(move || {
            thread::sleep(delay);
            if inner_clone.background_stop.load(Ordering::SeqCst) {
                return;
            }
            if let Ok(queue) = inner_clone.op_queue.lock() {
                if let Some(queue) = queue.as_ref() {
                    let op_id = next_op_id();
                    let _ = queue.submit(ProcessOp::StartServer {
                        op_id,
                        launch: launch_clone,
                        dimensions: SessionDimensions::default(),
                        activate: false,
                        response: None,
                    });
                    return;
                }
            }
            if let Err(error) = spawn_server_session_with_inner(
                &inner_clone,
                &launch_clone,
                SessionDimensions::default(),
            ) {
                let mut changed = false;
                if let Ok(mut runtime) = inner_clone.runtime_state.write() {
                    if let Some(session) = runtime.sessions.get_mut(&launch_clone.command_id) {
                        let dirty_before = session.dirty_generation;
                        session.status = SessionStatus::Failed;
                        session.exit = Some(SessionExitState {
                            code: None,
                            signal: None,
                            closed_by_user: false,
                            summary: format!("Auto-restart failed: {error}"),
                        });
                        session.mark_dirty();
                        changed = session.dirty_generation != dirty_before;
                    }
                }
                if changed {
                    bump_runtime_revision(&inner_clone);
                    emit_tracked_remote_runtime_snapshot(&inner_clone, &launch_clone.command_id);
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
    let args = vec![
        "-l".to_string(),
        "-c".to_string(),
        build_shell_command_line(command),
    ];

    (shell, args)
}

/// OpenSSH rejects key files with CRLF line endings or a missing final
/// newline — both are common artifacts of pasting a key into a text field.
fn sanitize_private_key(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    format!("{}\n", normalized.trim())
}

fn safe_key_file_name(connection_id: &str) -> String {
    connection_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn materialize_ssh_key_in(
    dir: &Path,
    connection: &SSHConnection,
) -> Result<Option<PathBuf>, String> {
    let Some(key) = connection
        .private_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
    else {
        return Ok(None);
    };

    let file_name = safe_key_file_name(&connection.id);
    if file_name.is_empty() {
        return Err("connection id is empty".to_string());
    }

    std::fs::create_dir_all(dir)
        .map_err(|error| format!("create {}: {error}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("set permissions on {}: {error}", dir.display()))?;
    }
    let path = dir.join(file_name);
    write_key_file(&path, &sanitize_private_key(key))?;
    if let Err(error) = lock_key_file_permissions(&path) {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(Some(path))
}

#[cfg(unix)]
fn write_key_file(path: &Path, contents: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(contents.as_bytes()))
        .map_err(|error| format!("write {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn write_key_file(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|error| format!("write {}: {error}", path.display()))
}

#[cfg(unix)]
fn lock_key_file_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("set permissions on {}: {error}", path.display()))
}

#[cfg(windows)]
fn lock_key_file_permissions(path: &Path) -> Result<(), String> {
    // Win32-OpenSSH refuses private keys readable by other accounts. Strip
    // inherited ACEs and grant only the current user.
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let username =
        std::env::var("USERNAME").map_err(|_| "resolve current user name".to_string())?;
    let output = std::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{username}:F"))
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("run icacls on {}: {error}", path.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "icacls failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn lock_key_file_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn build_ssh_launch_spec(
    app_state: &AppState,
    tab: &SessionTab,
    connection: &SSHConnection,
    key_file: Option<&Path>,
) -> SshLaunchSpec {
    let cwd = app_state
        .find_project(&tab.project_id)
        .map(|project| PathBuf::from(&project.root_path))
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));

    let mut args = vec![
        format!("{}@{}", connection.username.trim(), connection.host.trim()),
        "-p".to_string(),
        connection.port.to_string(),
    ];
    if let Some(key_file) = key_file {
        // No `-o IdentitiesOnly=yes` on purpose: the user prefers the saved
        // key but still wants agent/default keys as fallback.
        args.push("-i".to_string());
        args.push(key_file.display().to_string());
    }

    SshLaunchSpec {
        tab_id: tab.id.clone(),
        ssh_connection_id: connection.id.clone(),
        project_id: tab.project_id.clone(),
        cwd,
        program: "ssh".to_string(),
        args,
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
                bash_shell_args(settings.shell_integration_enabled),
            ),
        };
    }

    if cfg!(target_os = "macos") {
        // On macOS the default_terminal setting (Bash/Powershell/Cmd) doesn't apply.
        // resolve_shell_path honors mac_terminal_profile and falls back to $SHELL/zsh,
        // avoiding the bundled bash 3.2.
        let shell = resolve_shell_path(settings);
        return (shell, vec!["-l".to_string()]);
    }

    match settings.default_terminal.clone() {
        crate::models::DefaultTerminal::Bash => (
            "bash".to_string(),
            bash_shell_args(settings.shell_integration_enabled),
        ),
        _ => {
            let shell = resolve_shell_path(settings);
            (shell, vec!["-l".to_string()])
        }
    }
}

fn claude_shell_kind(shell_program: &str) -> ClaudeShellKind {
    let executable = shell_program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(shell_program)
        .to_ascii_lowercase();
    if matches!(
        executable.as_str(),
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
    ) {
        ClaudeShellKind::PowerShell
    } else if matches!(executable.as_str(), "cmd" | "cmd.exe") {
        ClaudeShellKind::Cmd
    } else {
        ClaudeShellKind::Posix
    }
}

fn claude_hook_base_root() -> PathBuf {
    std::env::temp_dir().join("devmanager").join("claude-hooks")
}

fn prepare_claude_overlay_process_root() -> PathBuf {
    let base = claude_hook_base_root();
    let _ = std::fs::create_dir_all(&base);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700));
    }
    cleanup_orphaned_claude_overlay_roots_at(&base, |pid, started_at| {
        platform_service::process_matches_identity(pid, started_at, None)
    });

    let pid = std::process::id();
    let started_at = platform_service::capture_process_identity(pid)
        .map(|identity| identity.started_at_unix_secs)
        .unwrap_or(0);
    let token = claude_overlay_owner_token();
    base.join(format!("owner-{pid}-{started_at}-{token}"))
}

fn claude_overlay_owner_token() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_ok() {
        let mut encoded = String::with_capacity(32);
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(encoded, "{byte:02x}");
        }
        return encoded;
    }
    let counter = CLAUDE_OVERLAY_OWNER_COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:032x}", time ^ counter)
}

fn parse_claude_overlay_owner(path: &Path) -> Option<(u32, u64)> {
    let name = path.file_name()?.to_str()?.strip_prefix("owner-")?;
    let mut fields = name.split('-');
    let pid = fields.next()?.parse().ok()?;
    let started_at = fields.next()?.parse().ok()?;
    let token = fields.next()?;
    if fields.next().is_some()
        || token.len() != 32
        || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some((pid, started_at))
}

fn cleanup_orphaned_claude_overlay_roots_at(
    base: &Path,
    mut owner_is_alive: impl FnMut(u32, u64) -> bool,
) -> usize {
    let Ok(entries) = std::fs::read_dir(base) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some((pid, started_at)) = parse_claude_overlay_owner(&path) else {
            continue;
        };
        // A zero start time cannot distinguish PID reuse. Preserve it rather
        // than risking another live DevManager instance.
        if started_at == 0 || owner_is_alive(pid, started_at) {
            continue;
        }
        if remove_owned_claude_overlay_root(&path) {
            removed += 1;
        }
    }
    removed
}

fn remove_owned_claude_overlay_root(process_root: &Path) -> bool {
    let Some(base) = process_root.parent() else {
        return false;
    };
    let Ok(metadata) = std::fs::symlink_metadata(process_root) else {
        return false;
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return false;
    }
    let (Ok(canonical_base), Ok(canonical_root)) =
        (base.canonicalize(), process_root.canonicalize())
    else {
        return false;
    };
    if canonical_root.parent() != Some(canonical_base.as_path()) {
        return false;
    }
    std::fs::remove_dir_all(canonical_root).is_ok()
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

    let _ = force_reap_session_processes_until_clear(inner, &session_id, Duration::from_secs(2));

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
        Some(session_change_notifier(inner.clone(), session_id.clone())),
        Some(session_output_notifier(inner.clone(), session_id.clone())),
    )?;

    if let Ok(mut sessions) = inner.sessions.lock() {
        sessions.insert(session_id.clone(), Arc::new(session));
    }

    let mut active_changed = false;
    if let Ok(mut runtime) = inner.runtime_state.write() {
        if runtime.active_session_id.is_none() {
            runtime.active_session_id = Some(session_id);
            active_changed = true;
        }
    }
    if active_changed {
        bump_runtime_revision(inner);
    }

    Ok(())
}

fn restore_interrupted_server_prompt(
    inner: &Arc<ProcessManagerInner>,
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
            Some(session_change_notifier(
                inner.clone(),
                session_id.to_string(),
            )),
            Some(session_output_notifier(
                inner.clone(),
                session_id.to_string(),
            )),
        )?;
        inner
            .sessions
            .lock()
            .map_err(|_| "Session store poisoned".to_string())?
            .insert(session_id.to_string(), Arc::new(session));
    }

    let mut changed = false;
    if let Ok(mut runtime) = inner.runtime_state.write() {
        if let Some(session) = runtime.sessions.get_mut(session_id) {
            let dirty_before = session.dirty_generation;
            session.cwd = cwd;
            session.dimensions = dimensions;
            session.activate_interactive_shell(
                shell_program,
                "Server interrupted with Ctrl+C. Terminal ready.",
            );
            changed = session.dirty_generation != dirty_before;
        }
    }
    if changed {
        bump_runtime_revision(inner);
        emit_tracked_remote_runtime_snapshot(inner, session_id);
    }

    Ok(())
}

fn mark_remote_session_dirty(inner: &Arc<ProcessManagerInner>, session_id: &str) {
    if let Ok(mut dirty) = inner.remote_dirty_sessions.lock() {
        dirty.insert(session_id.to_string());
    }
}

fn bump_runtime_revision(inner: &ProcessManagerInner) {
    inner.runtime_revision.fetch_add(1, Ordering::Relaxed);
}

fn current_runtime_generation(inner: &ProcessManagerInner, session_id: &str) -> Option<u64> {
    inner.runtime_state.read().ok().and_then(|runtime| {
        runtime
            .sessions
            .get(session_id)
            .map(|session| session.dirty_generation)
    })
}

fn remember_runtime_generation(inner: &ProcessManagerInner, session_id: &str, generation: u64) {
    if let Ok(mut observed) = inner.observed_runtime_generations.lock() {
        observed.insert(session_id.to_string(), generation);
    }
}

fn remember_current_runtime_generation(inner: &ProcessManagerInner, session_id: &str) {
    if let Some(generation) = current_runtime_generation(inner, session_id) {
        remember_runtime_generation(inner, session_id, generation);
    }
}

fn note_runtime_generation_change(inner: &ProcessManagerInner, session_id: &str) -> bool {
    let Some(generation) = current_runtime_generation(inner, session_id) else {
        return false;
    };
    let changed = inner
        .observed_runtime_generations
        .lock()
        .map(|mut observed| {
            if observed.get(session_id).copied() == Some(generation) {
                return false;
            }
            observed.insert(session_id.to_string(), generation);
            true
        })
        .unwrap_or(true);
    if changed {
        bump_runtime_revision(inner);
    }
    changed
}

fn emit_tracked_remote_runtime_snapshot(inner: &ProcessManagerInner, session_id: &str) {
    remember_current_runtime_generation(inner, session_id);
    emit_remote_runtime_snapshot(inner, session_id);
}

fn cleanup_claude_hook_session_if_matches(
    inner: &ProcessManagerInner,
    session_id: &str,
    expected: &ClaudeHookRegistration,
) -> bool {
    let removed = {
        let mut sessions = inner
            .claude_hook_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let matches = sessions.get(session_id).is_some_and(|session| {
            session.registration.nonce == expected.nonce
                && session.registration.generation == expected.generation
                && session.registration.stable_session_key == expected.stable_session_key
        });
        matches.then(|| sessions.remove(session_id)).flatten()
    };
    let Some(removed) = removed else {
        return false;
    };
    inner
        .claude_hook_registry
        .unregister_registration(&removed.registration);
    let _ = std::fs::remove_file(removed.settings_path);
    true
}

fn session_change_notifier(
    inner: Arc<ProcessManagerInner>,
    session_id: String,
) -> Arc<dyn Fn() + Send + Sync> {
    let claude_registration = inner
        .claude_hook_sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&session_id)
        .map(|session| session.registration.clone());
    Arc::new(move || {
        if note_runtime_generation_change(&inner, &session_id) {
            mark_remote_session_dirty(&inner, &session_id);
            emit_remote_runtime_snapshot(&inner, &session_id);
        }
        let terminal_exited = inner
            .runtime_state
            .read()
            .ok()
            .and_then(|runtime| {
                runtime
                    .sessions
                    .get(&session_id)
                    .map(|session| !session.status.is_live())
            })
            .unwrap_or(true);
        if terminal_exited {
            if let Some(registration) = claude_registration.as_ref() {
                cleanup_claude_hook_session_if_matches(&inner, &session_id, registration);
            }
        }
    })
}

fn session_output_notifier(
    inner: Arc<ProcessManagerInner>,
    session_id: String,
) -> Arc<dyn Fn(Vec<u8>, TerminalModeSnapshot) + Send + Sync> {
    Arc::new(move |bytes, mode| {
        if bytes.is_empty() {
            return;
        }
        emit_remote_session_event(
            &inner,
            RemoteSessionEvent::Output {
                session_id: session_id.clone(),
                bytes,
                mode,
            },
        );
    })
}

fn emit_remote_session_event(inner: &ProcessManagerInner, event: RemoteSessionEvent) {
    let handler = inner
        .remote_session_handler
        .read()
        .ok()
        .and_then(|handler| handler.clone());
    if let Some(handler) = handler {
        handler(event);
    }
}

fn emit_remote_runtime_snapshot(inner: &ProcessManagerInner, session_id: &str) {
    let runtime = inner
        .runtime_state
        .read()
        .ok()
        .and_then(|runtime| runtime.sessions.get(session_id).cloned());
    let Some(runtime) = runtime else {
        return;
    };
    emit_remote_session_event(
        inner,
        RemoteSessionEvent::Runtime {
            session_id: session_id.to_string(),
            runtime,
        },
    );
}

fn emit_remote_session_removed(inner: &ProcessManagerInner, session_id: &str) {
    emit_remote_session_event(
        inner,
        RemoteSessionEvent::Removed {
            session_id: session_id.to_string(),
        },
    );
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
    let scope = crate::persistence::runtime_session_scope();
    format!("{prefix}-{scope}-{millis:x}-{counter:x}")
}

fn next_ssh_session_id(connection_id: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let counter = SSH_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let scope = crate::persistence::runtime_session_scope();
    format!("{connection_id}-{scope}-{millis:x}-{counter:x}")
}

fn process_manager_from_inner(inner: Arc<ProcessManagerInner>) -> ProcessManager {
    let op_queue = inner
        .op_queue
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .expect("process op queue missing");
    let claude_overlay_owner = inner
        .claude_overlay_owner
        .lock()
        .ok()
        .and_then(|owner| owner.upgrade())
        .expect("Claude overlay owner missing");
    ProcessManager {
        inner,
        op_queue,
        _claude_overlay_owner: claude_overlay_owner,
    }
}

pub(crate) fn execute_process_op_inner(
    inner: &Arc<ProcessManagerInner>,
    op: ProcessOp,
) -> ProcessOpCompletion {
    let op_id = op.op_id();
    let target_id = op.target_id();
    let manager = process_manager_from_inner(inner.clone());
    let (kind, result, context, remote_response) = match op {
        ProcessOp::StartServer {
            launch,
            dimensions,
            activate,
            response,
            ..
        } => {
            if activate {
                manager.set_active_session(launch.command_id.clone());
            }
            let result = spawn_server_session_with_inner(inner, &launch, dimensions).map_err(|error| {
                manager.update_session_state(&launch.command_id, |state| {
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
            });
            if result.is_ok() {
                manager.update_session_state(&launch.command_id, |state| {
                    state.configure_server(launch.clone());
                });
            }
            (
                ProcessOpKind::StartServer,
                result.map(|_| ()),
                ProcessOpContext {
                    session_id: Some(launch.command_id.clone()),
                    focus: activate,
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::StopServer {
            command_id,
            wait,
            response,
            ..
        } => {
            let result = if wait.is_zero() {
                manager.stop_server(&command_id).map(|_| ())
            } else {
                if manager.stop_server_and_wait(&command_id, wait) {
                    Ok(())
                } else {
                    Err(format!("Failed to stop `{command_id}` cleanly."))
                }
            };
            (
                ProcessOpKind::StopServer,
                result,
                ProcessOpContext {
                    session_id: Some(command_id.clone()),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::RestartServer {
            launch,
            dimensions,
            banner,
            clear_logs,
            response,
            ..
        } => {
            let command_id = launch.command_id.clone();
            let result = (|| {
                if !manager.stop_server_and_wait(&command_id, Duration::from_secs(5)) {
                    return Err(format!(
                        "Managed process `{command_id}` did not stop cleanly."
                    ));
                }
                manager.set_active_session(command_id.clone());
                if let Ok(session) = manager.get_session(&command_id) {
                    if clear_logs {
                        session.clear_virtual_output();
                    }
                    session.write_virtual_text(&format!(
                        "{}\x1b[33m{banner}\x1b[0m\r\n",
                        if clear_logs { "" } else { "\r\n" }
                    ));
                    session.restart_command(
                        launch.cwd.clone(),
                        dimensions,
                        launch.program.clone(),
                        launch.args.clone(),
                        launch.env.clone(),
                        launch.log_file_path.clone(),
                        true,
                    )?;
                    manager.update_session_state(&command_id, |state| {
                        state.configure_server(launch.clone());
                    });
                    return Ok(());
                }
                spawn_server_session_with_inner(inner, &launch, dimensions)?;
                let _ = manager.write_virtual_text(
                    &command_id,
                    &format!(
                        "{}\x1b[33m{banner}\x1b[0m\r\n",
                        if clear_logs { "" } else { "\r\n" }
                    ),
                );
                manager.update_session_state(&command_id, |state| {
                    state.configure_server(launch.clone());
                });
                Ok(())
            })();
            (
                ProcessOpKind::RestartServer,
                result,
                ProcessOpContext {
                    session_id: Some(command_id),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::KillPortAndRestart {
            command_id,
            port,
            launch,
            dimensions,
            banner,
            response,
            ..
        } => {
            let result = (|| {
                let is_active = inner
                    .runtime_state
                    .read()
                    .ok()
                    .and_then(|runtime| {
                        runtime.sessions.get(&command_id).map(|session| session.status.is_live())
                    })
                    .unwrap_or(false);
                if is_active && !manager.stop_server_and_wait(&command_id, Duration::from_secs(5)) {
                    return Err(format!(
                        "Managed process `{command_id}` did not stop cleanly."
                    ));
                }
                crate::services::ports_service::kill_port(port)?;
                spawn_server_session_with_inner(inner, &launch, dimensions)?;
                let _ = manager.write_virtual_text(
                    &command_id,
                    &format!("\x1b[33m{banner}\x1b[0m\r\n"),
                );
                manager.update_session_state(&command_id, |state| {
                    state.configure_server(launch.clone());
                });
                Ok(())
            })();
            (
                ProcessOpKind::KillPortAndRestart,
                result,
                ProcessOpContext {
                    session_id: Some(command_id.clone()),
                    port: Some(port),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::StartSsh {
            launch,
            session_id,
            dimensions,
            key_warning,
            response,
            ..
        } => {
            let result = spawn_ssh_session_with_inner(inner, &launch, &session_id, dimensions);
            if let Some(error) = key_warning {
                let _ = manager.write_virtual_text(
                    &session_id,
                    &format!(
                        "[devmanager] Couldn't prepare the saved SSH key ({error}); trying password/agent auth instead.\r\n"
                    ),
                );
            }
            (
                ProcessOpKind::StartSsh,
                result,
                ProcessOpContext {
                    session_id: Some(session_id),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::RestartSsh {
            close_session_id,
            launch,
            session_id,
            dimensions,
            key_warning,
            response,
            ..
        } => {
            if let Some(close_id) = close_session_id {
                let _ = manager.close_session(&close_id);
                manager.forget_session(&close_id);
            }
            let result = spawn_ssh_session_with_inner(inner, &launch, &session_id, dimensions);
            if let Some(error) = key_warning {
                let _ = manager.write_virtual_text(
                    &session_id,
                    &format!(
                        "[devmanager] Couldn't prepare the saved SSH key ({error}); trying password/agent auth instead.\r\n"
                    ),
                );
            }
            (
                ProcessOpKind::RestartSsh,
                result,
                ProcessOpContext {
                    session_id: Some(session_id),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::CloseSsh {
            session_id,
            response,
            ..
        } => {
            let result = if let Some(session_id) = session_id {
                let _ = manager.close_session(&session_id);
                manager.forget_session(&session_id);
                Ok(())
            } else {
                Ok(())
            };
            (
                ProcessOpKind::CloseSsh,
                result,
                ProcessOpContext::default(),
                response,
            )
        }
        ProcessOp::SpawnAi {
            launch,
            session_id,
            dimensions,
            response,
            ..
        } => {
            let result = spawn_ai_session_with_inner(inner, &launch, &session_id, dimensions);
            (
                ProcessOpKind::SpawnAi,
                result,
                ProcessOpContext {
                    session_id: Some(session_id),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::RestartAi {
            close_session_id,
            launch,
            session_id,
            dimensions,
            response,
            ..
        } => {
            if let Some(close_id) = close_session_id {
                let _ = manager.close_session(&close_id);
                manager.forget_session(&close_id);
            }
            let result = spawn_ai_session_with_inner(inner, &launch, &session_id, dimensions);
            (
                ProcessOpKind::RestartAi,
                result,
                ProcessOpContext {
                    session_id: Some(session_id),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::CloseAi {
            session_id,
            response,
            ..
        } => {
            let _ = manager.close_session(&session_id);
            manager.forget_session(&session_id);
            (
                ProcessOpKind::CloseAi,
                Ok(()),
                ProcessOpContext {
                    session_id: Some(session_id),
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::StopAll {
            command_ids,
            wait,
            response,
            ..
        } => {
            let mut failures = Vec::new();
            for command_id in &command_ids {
                if wait.is_zero() {
                    if let Err(error) = manager.stop_server(command_id) {
                        failures.push(error);
                    }
                } else if !manager.stop_server_and_wait(command_id, wait) {
                    failures.push(format!("Failed to stop `{command_id}` cleanly."));
                }
            }
            let result = if failures.is_empty() {
                Ok(())
            } else {
                Err(failures.join(" "))
            };
            (
                ProcessOpKind::StopAll,
                result,
                ProcessOpContext::default(),
                response,
            )
        }
        ProcessOp::Shutdown { timeout, .. } => {
            let report = shutdown_managed_processes_inner(inner, timeout);
            (
                ProcessOpKind::Shutdown,
                if report.remaining_live_sessions == 0 && report.remaining_tracked_pids == 0 {
                    Ok(())
                } else {
                    Err(format!(
                        "Shutdown left {} live session(s) and {} tracked pid(s).",
                        report.remaining_live_sessions, report.remaining_tracked_pids
                    ))
                },
                ProcessOpContext {
                    shutdown_report: Some(report),
                    ..Default::default()
                },
                None,
            )
        }
        ProcessOp::KillProcess {
            session_id,
            pid,
            response,
            ..
        } => {
            let outcome = kill_session_process_inner(inner, &session_id, pid, false);
            let (result, message) = match outcome {
                Ok(KillProcessOutcome::Killed) => {
                    (Ok(()), Some(format!("Killed process {pid}.")))
                }
                Ok(KillProcessOutcome::AlreadyGone) => (
                    Ok(()),
                    Some(format!("Process {pid} was already gone.")),
                ),
                Err(error) => (Err(error), None),
            };
            (
                ProcessOpKind::KillProcess,
                result,
                ProcessOpContext {
                    session_id: Some(session_id),
                    message,
                    ..Default::default()
                },
                response,
            )
        }
        ProcessOp::KillProcessTree {
            session_id,
            pid,
            response,
            ..
        } => {
            let outcome = kill_session_process_inner(inner, &session_id, pid, true);
            let (result, message) = match outcome {
                Ok(KillProcessOutcome::Killed) => (
                    Ok(()),
                    Some(format!("Killed process tree rooted at {pid}.")),
                ),
                Ok(KillProcessOutcome::AlreadyGone) => (
                    Ok(()),
                    Some(format!("Process tree rooted at {pid} was already gone.")),
                ),
                Err(error) => (Err(error), None),
            };
            (
                ProcessOpKind::KillProcessTree,
                result,
                ProcessOpContext {
                    session_id: Some(session_id),
                    message,
                    ..Default::default()
                },
                response,
            )
        }
    };

    ProcessOpCompletion {
        op_id,
        kind,
        target_id,
        result,
        context,
        remote_response,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillProcessOutcome {
    Killed,
    AlreadyGone,
}

fn verified_session_process_identity(
    inner: &Arc<ProcessManagerInner>,
    session_id: &str,
    pid: u32,
) -> Option<platform_service::ProcessIdentity> {
    for entry in pid_file::active_tracked_processes_for_session(session_id) {
        if entry.pid == pid
            && platform_service::process_matches_identity(
                entry.pid,
                entry.started_at_unix_secs,
                entry.process_name.as_deref(),
            )
        {
            return Some(platform_service::ProcessIdentity {
                pid: entry.pid,
                started_at_unix_secs: entry.started_at_unix_secs,
                process_name: entry.process_name.clone(),
            });
        }
        for descendant in &entry.descendant_processes {
            if descendant.pid == pid
                && platform_service::process_matches_identity(
                    descendant.pid,
                    descendant.started_at_unix_secs,
                    descendant.process_name.as_deref(),
                )
            {
                return Some(platform_service::ProcessIdentity {
                    pid: descendant.pid,
                    started_at_unix_secs: descendant.started_at_unix_secs,
                    process_name: descendant.process_name.clone(),
                });
            }
        }
        if platform_service::process_matches_identity(
            entry.pid,
            entry.started_at_unix_secs,
            entry.process_name.as_deref(),
        ) {
            for descendant in platform_service::collect_descendant_process_identities(entry.pid) {
                if descendant.pid == pid {
                    return Some(descendant);
                }
            }
        }
    }

    if live_runtime_root_pid(inner, session_id) == Some(pid) {
        return platform_service::capture_process_identity(pid);
    }
    if let Some(root_pid) = live_runtime_root_pid(inner, session_id) {
        for descendant in platform_service::collect_descendant_process_identities(root_pid) {
            if descendant.pid == pid {
                return Some(descendant);
            }
        }
    }
    None
}

fn kill_session_process_inner(
    inner: &Arc<ProcessManagerInner>,
    session_id: &str,
    pid: u32,
    kill_tree: bool,
) -> Result<KillProcessOutcome, String> {
    let Some(expected) = verified_session_process_identity(inner, session_id, pid) else {
        return Err(format!(
            "Process {pid} is not part of session `{session_id}`."
        ));
    };
    if !platform_service::process_matches_identity(
        pid,
        expected.started_at_unix_secs,
        expected.process_name.as_deref(),
    ) {
        return Err(format!(
            "Process {pid} no longer matches the tracked identity for session `{session_id}`."
        ));
    }
    if !platform_service::is_pid_running(pid) {
        let _ = pid_file::prune_inactive_entries();
        bump_runtime_revision(inner);
        return Ok(KillProcessOutcome::AlreadyGone);
    }
    let result = if kill_tree {
        platform_service::kill_process_tree(pid)
    } else {
        platform_service::kill_process(pid)
    };
    let _ = pid_file::prune_inactive_entries();
    result?;
    let remaining = pid_file::active_tracked_pids_for_session(session_id);
    if remaining.is_empty() && !live_runtime_root_running(inner, session_id) {
        mark_session_reaped(inner, session_id);
    } else {
        bump_runtime_revision(inner);
        emit_tracked_remote_runtime_snapshot(inner, session_id);
    }
    Ok(KillProcessOutcome::Killed)
}

fn spawn_ssh_session_with_inner(
    inner: &Arc<ProcessManagerInner>,
    launch: &SshLaunchSpec,
    session_id: &str,
    dimensions: SessionDimensions,
) -> Result<(), String> {
    let manager = process_manager_from_inner(inner.clone());
    if manager.session_exists(session_id) {
        return Ok(());
    }
    let _ = force_reap_session_processes_until_clear(inner, session_id, Duration::from_secs(2));
    let session = TerminalSession::spawn_command(
        session_id.to_string(),
        launch.cwd.clone(),
        dimensions,
        launch.program.clone(),
        launch.args.clone(),
        HashMap::new(),
        inner
            .scrollback_lines
            .read()
            .map(|lines| *lines)
            .unwrap_or(10_000),
        None,
        inner.runtime_state.clone(),
        inner.debug_enabled,
        Some(session_change_notifier(inner.clone(), session_id.to_string())),
        Some(session_output_notifier(inner.clone(), session_id.to_string())),
    )
    .map_err(|error| {
        manager.update_session_state(session_id, |state| {
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
    if let Ok(mut sessions) = inner.sessions.lock() {
        sessions.insert(session_id.to_string(), Arc::new(session));
    }
    Ok(())
}

fn spawn_ai_session_with_inner(
    inner: &Arc<ProcessManagerInner>,
    launch: &AiLaunchSpec,
    session_id: &str,
    dimensions: SessionDimensions,
) -> Result<(), String> {
    let manager = process_manager_from_inner(inner.clone());
    if manager.session_exists(session_id) {
        return Ok(());
    }
    let _ = force_reap_session_processes_until_clear(inner, session_id, Duration::from_secs(2));
    let session = TerminalSession::spawn_command(
        session_id.to_string(),
        launch.cwd.clone(),
        dimensions,
        launch.shell_program.clone(),
        launch.shell_args.clone(),
        HashMap::new(),
        inner
            .scrollback_lines
            .read()
            .map(|lines| *lines)
            .unwrap_or(10_000),
        None,
        inner.runtime_state.clone(),
        inner.debug_enabled,
        Some(session_change_notifier(inner.clone(), session_id.to_string())),
        Some(session_output_notifier(inner.clone(), session_id.to_string())),
    )
    .map_err(|error| {
        manager.cleanup_claude_hook_session(session_id);
        manager.update_session_state(session_id, |state| {
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
    if let Ok(mut sessions) = inner.sessions.lock() {
        sessions.insert(session_id.to_string(), session.clone());
    }
    thread::sleep(Duration::from_millis(AI_COMMAND_INJECTION_DELAY_MS));
    let _ = session.write_text(&(launch.startup_command.clone() + "\r\n"));
    Ok(())
}

fn shutdown_managed_processes_inner(
    inner: &Arc<ProcessManagerInner>,
    timeout: Duration,
) -> ManagedShutdownReport {
    let manager = process_manager_from_inner(inner.clone());
    let session_ids = manager.live_session_ids();
    for session_id in &session_ids {
        let _ = manager.request_session_close(session_id, false);
    }

    let started_at = Instant::now();
    let mut active_tracked_processes = loop {
        let _ = pid_file::prune_inactive_entries();
        let remaining_live_sessions = manager.live_session_count();
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
    if manager.live_session_count() > 0 || !active_tracked_processes.is_empty() {
        for session_id in manager.live_session_ids() {
            forced_kill_pids += force_reap_session_processes(inner, &session_id);
        }

        let mut pids_to_kill = manager.live_session_pids();
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
            let remaining_live_sessions = manager.live_session_count();
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
        remaining_live_sessions: manager.live_session_count(),
        remaining_tracked_pids: pid_file::active_tracked_pids().len(),
    };
    if report.remaining_live_sessions == 0 && report.remaining_tracked_pids == 0 {
        pid_file::clear_all();
    }
    manager.drain_claude_hook_adapter();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        AppConfig, Project, ProjectFolder, RunCommand, SessionTab, Settings, TabType,
    };
    use crate::services::pid_file;
    use std::fs;
    use std::thread;

    #[test]
    fn output_notifier_forwards_the_native_terminal_mode() {
        let manager = ProcessManager::new();
        let (tx, rx) = std::sync::mpsc::channel();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            if let RemoteSessionEvent::Output { mode, .. } = event {
                tx.send(mode).expect("mode receiver should remain open");
            }
        })));
        let notifier = session_output_notifier(manager.inner.clone(), "alpha".to_string());
        let mode = crate::terminal::session::TerminalModeSnapshot {
            alternate_screen: true,
            mouse_report_click: true,
            ..crate::terminal::session::TerminalModeSnapshot::default()
        };

        notifier(b"output".to_vec(), mode);

        assert_eq!(rx.recv_timeout(Duration::from_millis(100)), Ok(mode));
    }

    #[test]
    fn remote_event_callbacks_can_replace_the_handler_without_deadlocking() {
        let manager = ProcessManager::new();
        let callback_manager = manager.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        manager.set_remote_session_handler(Some(Arc::new(move |_| {
            callback_manager.set_remote_session_handler(None);
            tx.send(()).unwrap();
        })));
        let notifier = session_output_notifier(manager.inner.clone(), "lock-test".to_string());

        thread::spawn(move || {
            notifier(
                b"output".to_vec(),
                crate::terminal::session::TerminalModeSnapshot::default(),
            );
        });

        assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok(()));
    }

    #[test]
    fn claude_launch_preparation_is_private_and_cleanup_is_session_scoped() {
        let temp = temp_test_dir("claude-hook-launch");
        let manager = ProcessManager::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        })));
        let mut launch = AiLaunchSpec {
            tab_id: "claude-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude --model sonnet".to_string(),
        };

        manager.prepare_claude_launch_for_session(&mut launch, "claude-session", &temp);

        assert!(launch.startup_command.contains("--settings"));
        assert_eq!(manager.inner.claude_hook_registry.registration_count(), 1);
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::ClaudeAdapterRegistered { identity }
                if identity.pty_session_id == "claude-session"
                    && identity.stable_session_key == crate::remote::presentation::StableSessionKey::from_tab("claude-tab")
        )));
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::AdapterHealth {
                stable_session_key,
                health: crate::remote::presentation::SemanticAdapterHealth::Degraded,
            } if stable_session_key == &crate::remote::presentation::StableSessionKey::from_tab("claude-tab")
        )));
        let (registration, settings_path) = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("claude-session")
            .map(|session| (session.registration.clone(), session.settings_path.clone()))
            .expect("Claude hook session");
        assert!(settings_path.is_file());
        assert!(!settings_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&registration.nonce));

        events.lock().unwrap().clear();
        let endpoint = manager.claude_hook_endpoint().unwrap();
        ureq::post(&endpoint)
            .header("x-devmanager-claude-nonce", &registration.nonce)
            .send(br#"{"hook_event_name":"SessionStart","session_id":"provider-session","source":"startup"}"#)
            .unwrap();
        let started_at = Instant::now();
        while started_at.elapsed() < Duration::from_secs(2)
            && !events.lock().unwrap().iter().any(|event| matches!(
                event,
                RemoteSessionEvent::AdapterHealth {
                    stable_session_key,
                    health: crate::remote::presentation::SemanticAdapterHealth::Healthy,
                } if stable_session_key == &crate::remote::presentation::StableSessionKey::from_tab("claude-tab")
            ))
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::AdapterHealth {
                stable_session_key,
                health: crate::remote::presentation::SemanticAdapterHealth::Healthy,
            } if stable_session_key == &crate::remote::presentation::StableSessionKey::from_tab("claude-tab")
        )));
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::ClaudeSemantic { identity, draft }
                if identity.pty_session_id == "claude-session"
                    && matches!(&draft.kind, crate::remote::presentation::SemanticEventKind::Status { state, .. } if state == "started")
        )));

        manager.cleanup_claude_hook_session("claude-session");

        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::ClaudeAdapterRemoved { identity }
                if identity.pty_session_id == "claude-session"
        )));

        assert_eq!(manager.inner.claude_hook_registry.registration_count(), 0);
        assert!(!settings_path.exists());
    }

    #[test]
    fn logical_session_end_survives_until_exact_pty_generation_exit() {
        let temp = temp_test_dir("claude-hook-replacement");
        let manager = ProcessManager::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        })));
        let mut old_launch = AiLaunchSpec {
            tab_id: "shared-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut old_launch, "old-session", &temp);
        manager.ensure_runtime_entry("old-session", temp.clone(), SessionDimensions::default());
        manager.update_session_state("old-session", |state| {
            state.status = SessionStatus::Running;
        });
        let old_exit_notifier =
            session_change_notifier(manager.inner.clone(), "old-session".to_string());
        let (old_registration, old_settings_path) = {
            let sessions = manager.inner.claude_hook_sessions.lock().unwrap();
            let old = sessions.get("old-session").unwrap();
            (old.registration.clone(), old.settings_path.clone())
        };
        let mut replacement = old_launch.clone();
        replacement.startup_command = "claude".to_string();
        manager.prepare_claude_launch_for_session(&mut replacement, "new-session", &temp);
        let new_settings_path = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("new-session")
            .map(|session| session.settings_path.clone())
            .unwrap();
        events.lock().unwrap().clear();

        let endpoint = manager.claude_hook_endpoint().unwrap();
        let response = ureq::post(&endpoint)
            .header("x-devmanager-claude-nonce", &old_registration.nonce)
            .send(br#"{"hook_event_name":"SessionEnd","reason":"clear"}"#)
            .unwrap();

        assert_eq!(response.status().as_u16(), 204);
        assert!(manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .contains_key("old-session"));
        assert_eq!(manager.inner.claude_hook_registry.registration_count(), 2);
        assert!(old_settings_path.exists());
        assert!(new_settings_path.exists());
        assert!(!events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::AdapterHealth {
                stable_session_key,
                health: crate::remote::presentation::SemanticAdapterHealth::Degraded,
            } if stable_session_key == &crate::remote::presentation::StableSessionKey::from_tab("shared-tab")
        )));

        manager.update_session_state("old-session", |state| {
            state.status = SessionStatus::Exited;
        });
        old_exit_notifier();

        assert!(!manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .contains_key("old-session"));
        assert_eq!(manager.inner.claude_hook_registry.registration_count(), 1);
        assert!(!old_settings_path.exists());
        assert!(new_settings_path.exists());
        manager.cleanup_claude_hook_session("new-session");
    }

    #[test]
    fn late_old_pty_exit_cannot_remove_replacement_for_reused_session_id() {
        let temp = temp_test_dir("claude-hook-reused-session");
        let manager = ProcessManager::new();
        let mut launch = AiLaunchSpec {
            tab_id: "shared-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut launch, "shared-session", &temp);
        manager.ensure_runtime_entry("shared-session", temp.clone(), SessionDimensions::default());
        manager.update_session_state("shared-session", |state| {
            state.status = SessionStatus::Running;
        });
        let old_exit_notifier =
            session_change_notifier(manager.inner.clone(), "shared-session".to_string());
        let old_generation = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("shared-session")
            .unwrap()
            .registration
            .generation;

        launch.startup_command = "claude".to_string();
        manager.prepare_claude_launch_for_session(&mut launch, "shared-session", &temp);
        let (replacement_generation, replacement_path) = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("shared-session")
            .map(|session| {
                (
                    session.registration.generation,
                    session.settings_path.clone(),
                )
            })
            .unwrap();
        assert!(replacement_generation > old_generation);

        manager.update_session_state("shared-session", |state| {
            state.status = SessionStatus::Exited;
        });
        old_exit_notifier();

        let sessions = manager.inner.claude_hook_sessions.lock().unwrap();
        assert_eq!(
            sessions
                .get("shared-session")
                .unwrap()
                .registration
                .generation,
            replacement_generation
        );
        drop(sessions);
        assert!(replacement_path.exists());
        manager.cleanup_claude_hook_session("shared-session");
    }

    #[test]
    fn unexpected_pty_exit_without_session_end_cleans_registration() {
        let temp = temp_test_dir("claude-hook-unexpected-exit");
        let manager = ProcessManager::new();
        let mut launch = AiLaunchSpec {
            tab_id: "unexpected-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut launch, "unexpected-session", &temp);
        manager.ensure_runtime_entry(
            "unexpected-session",
            temp.clone(),
            SessionDimensions::default(),
        );
        manager.update_session_state("unexpected-session", |state| {
            state.status = SessionStatus::Running;
        });
        let exit_notifier =
            session_change_notifier(manager.inner.clone(), "unexpected-session".to_string());
        let settings_path = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("unexpected-session")
            .unwrap()
            .settings_path
            .clone();

        manager.update_session_state("unexpected-session", |state| {
            state.status = SessionStatus::Crashed;
        });
        exit_notifier();

        assert_eq!(manager.inner.claude_hook_registry.registration_count(), 0);
        assert!(!manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .contains_key("unexpected-session"));
        assert!(!settings_path.exists());
    }

    #[test]
    fn expired_claude_registration_degrades_the_exact_session_and_cleans_tracking() {
        let temp = temp_test_dir("claude-hook-expiry");
        let manager = ProcessManager::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        manager.set_remote_session_handler(Some(Arc::new(move |event| {
            observed.lock().unwrap().push(event);
        })));
        let mut launch = AiLaunchSpec {
            tab_id: "expiring-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut launch, "expiring-session", &temp);
        events.lock().unwrap().clear();

        let removed = manager
            .inner
            .claude_hook_registry
            .cleanup_expired_at(Instant::now() + Duration::from_secs(6 * 60));

        assert_eq!(removed, 1);
        assert!(!manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .contains_key("expiring-session"));
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RemoteSessionEvent::AdapterHealth {
                stable_session_key,
                health: crate::remote::presentation::SemanticAdapterHealth::Degraded,
            } if stable_session_key == &crate::remote::presentation::StableSessionKey::from_tab("expiring-tab")
        )));
    }

    #[test]
    fn claude_spawn_failure_immediately_removes_registration_and_settings() {
        let temp = temp_test_dir("claude-hook-spawn-failure");
        let _pid_file_guard = pid_file::use_test_pid_file(temp.join("running-pids.json"));
        let manager = ProcessManager::new();
        let mut launch = AiLaunchSpec {
            tab_id: "failure-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: temp.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut launch, "failure-session", &temp);
        let settings_path = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("failure-session")
            .map(|session| session.settings_path.clone())
            .unwrap();
        launch.shell_program = "definitely-missing-devmanager-shell".to_string();

        let result = spawn_ai_session_with_inner(
            &manager.inner,
            &launch,
            "failure-session",
            SessionDimensions::default(),
        );

        assert!(result.is_err());
        assert_eq!(manager.inner.claude_hook_registry.registration_count(), 0);
        assert!(!manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .contains_key("failure-session"));
        assert!(!settings_path.exists());
    }

    #[test]
    fn claude_overlay_orphan_sweep_never_removes_a_live_or_unverifiable_owner() {
        let base = temp_test_dir("claude-hook-orphan-sweep");
        let live = base.join("owner-101-1001-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let dead = base.join("owner-202-2002-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let unverifiable = base.join("owner-malformed");
        for root in [&live, &dead, &unverifiable] {
            fs::create_dir_all(root).unwrap();
            fs::write(root.join("copied-settings.json"), b"secret").unwrap();
        }

        let removed = cleanup_orphaned_claude_overlay_roots_at(&base, |pid, started_at| {
            pid == 101 && started_at == 1001
        });

        assert_eq!(removed, 1);
        assert!(live.exists(), "a live DevManager instance owns this root");
        assert!(!dead.exists(), "a verified dead owner is safe to clean");
        assert!(
            unverifiable.exists(),
            "malformed ownership must fail closed rather than risk another instance"
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn explicit_claude_adapter_drain_removes_all_settings_and_the_process_root() {
        let manager = ProcessManager::new();
        let process_root = manager.inner.claude_hook_temp_root.clone();
        let mut launch = AiLaunchSpec {
            tab_id: "drain-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: process_root.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut launch, "drain-session", &process_root);
        let settings_path = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("drain-session")
            .unwrap()
            .settings_path
            .clone();
        assert!(settings_path.exists());

        manager.drain_claude_hook_adapter();

        assert_eq!(manager.inner.claude_hook_registry.registration_count(), 0);
        assert!(manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .is_empty());
        assert!(!settings_path.exists());
        assert!(!process_root.exists());
    }

    #[test]
    fn dropping_the_last_process_manager_handle_drains_claude_overlays() {
        let manager = ProcessManager::new();
        let process_root = manager.inner.claude_hook_temp_root.clone();
        let mut launch = AiLaunchSpec {
            tab_id: "drop-drain-tab".to_string(),
            project_id: "project".to_string(),
            tool: SessionKind::Claude,
            cwd: process_root.clone(),
            shell_program: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            startup_command: "claude".to_string(),
        };
        manager.prepare_claude_launch_for_session(&mut launch, "drop-drain-session", &process_root);
        let settings_path = manager
            .inner
            .claude_hook_sessions
            .lock()
            .unwrap()
            .get("drop-drain-session")
            .unwrap()
            .settings_path
            .clone();
        assert!(settings_path.exists());

        drop(manager);

        assert!(!settings_path.exists());
        assert!(!process_root.exists());
    }

    #[test]
    fn clear_virtual_output_resets_terminal_snapshot() {
        let cwd = temp_test_dir("clear-virtual-output");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "test-shell";

        manager
            .spawn_shell_session(session_id, &cwd, SessionDimensions::default(), None, None)
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
            let cwd = temp_test_dir(if clear_logs_on_restart {
                "restart-clear-logs"
            } else {
                "restart-preserve-logs"
            });
            let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
            let manager = ProcessManager::new();
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
            for _ in 0..50 {
                let _ = manager.drain_process_op_completions();
                if manager
                    .session_view(command_id)
                    .map(|view| screen_text(&view).contains("Restarting"))
                    .unwrap_or(false)
                {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
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
            .spawn_shell_session(session_id, &cwd, SessionDimensions::default(), None, None)
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

    #[test]
    fn ai_session_does_not_need_restore_during_fresh_unattached_startup_gap() {
        let now = Instant::now();
        let mut session = SessionRuntimeState::new(
            "claude-session",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.session_kind = SessionKind::Claude;
        session.status = SessionStatus::Starting;

        assert!(!ai_session_needs_restore(Some(&session), false, now));

        session.status = SessionStatus::Running;
        session.started_at = Some(now);
        assert!(!ai_session_needs_restore(Some(&session), false, now));

        session.started_at = Some(now - Duration::from_secs(31));
        assert!(ai_session_needs_restore(Some(&session), false, now));

        assert!(!ai_session_needs_restore(Some(&session), true, now));
        assert!(ai_session_needs_restore(None, false, now));
    }

    #[test]
    fn detects_blocking_external_editor_children() {
        let descendants = vec![
            platform_service::ProcessIdentity {
                pid: 11,
                started_at_unix_secs: 1,
                process_name: Some("node.exe".to_string()),
            },
            platform_service::ProcessIdentity {
                pid: 12,
                started_at_unix_secs: 1,
                process_name: Some("Code.exe".to_string()),
            },
        ];
        assert!(is_blocking_external_editor(&descendants));

        let non_editor_descendants = vec![platform_service::ProcessIdentity {
            pid: 21,
            started_at_unix_secs: 1,
            process_name: Some("node.exe".to_string()),
        }];
        assert!(!is_blocking_external_editor(&non_editor_descendants));
    }

    #[test]
    fn reaper_targets_tracked_descendant_when_root_is_gone() {
        let cwd = temp_test_dir("reaper-dead-root-descendant");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let current = platform_service::capture_process_identity(std::process::id())
            .expect("current process identity");
        pid_file::track_session_process(pid_file::ManagedProcessRecord {
            session_id: "server-cmd".to_string(),
            pid: u32::MAX,
            started_at_unix_secs: 1,
            process_name: Some("missing-root.exe".to_string()),
            session_kind: "server".to_string(),
            program: "cmd".to_string(),
            project_id: Some("project-1".to_string()),
            command_id: Some("server-cmd".to_string()),
            tab_id: None,
            descendant_processes: vec![pid_file::TrackedProcessIdentity {
                pid: current.pid,
                started_at_unix_secs: current.started_at_unix_secs,
                process_name: current.process_name,
            }],
        })
        .unwrap();

        let pids = collect_session_reap_pids(&manager.inner, "server-cmd");

        assert_eq!(pids, vec![std::process::id()]);
    }

    #[test]
    fn reaper_marks_stopping_session_stopped_after_processes_clear() {
        let cwd = temp_test_dir("reaper-stopped-session");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        manager.register_runtime_session(SessionRuntimeState::new(
            "alpha",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        ));
        manager.update_session_state("alpha", |session| {
            session.status = SessionStatus::Stopping;
            session.pid = None;
            session.mark_dirty();
        });

        manager.reap_session_processes_until_clear("alpha", Duration::from_millis(1));

        let runtime = manager.runtime_state();
        assert_eq!(
            runtime.sessions.get("alpha").map(|session| session.status),
            Some(SessionStatus::Stopped)
        );
    }

    #[test]
    fn close_tab_removes_ssh_tab_and_stops_session() {
        let cwd = temp_test_dir("close-ssh-tab");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let mut app_state = AppState::default();
        app_state.open_tabs.push(SessionTab {
            id: "ssh-tab".to_string(),
            tab_type: TabType::Ssh,
            project_id: "project-1".to_string(),
            command_id: None,
            pty_session_id: Some("ssh-session".to_string()),
            label: Some("SSH".to_string()),
            ssh_connection_id: Some("ssh-1".to_string()),
        });
        manager.register_runtime_session(SessionRuntimeState::new(
            "ssh-session",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        ));

        manager.close_tab(&mut app_state, "ssh-tab").unwrap();
        for _ in 0..50 {
            let _ = manager.drain_process_op_completions();
            let status = manager
                .runtime_state()
                .sessions
                .get("ssh-session")
                .map(|session| session.status);
            if matches!(
                status,
                Some(SessionStatus::Stopped) | Some(SessionStatus::Failed) | None
            ) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }

        let runtime = manager.runtime_state();
        assert!(app_state.find_tab("ssh-tab").is_none());
        let status = runtime
            .sessions
            .get("ssh-session")
            .map(|session| session.status);
        assert!(
            matches!(
                status,
                Some(SessionStatus::Stopped) | Some(SessionStatus::Failed) | None
            ),
            "expected ssh session to stop or be removed, got {status:?}"
        );
    }

    #[test]
    fn schedule_start_server_returns_immediately() {
        let cwd = temp_test_dir("schedule-start-immediate");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let mut app_state = app_state_with_server(&cwd, true);
        let started = Instant::now();
        manager
            .start_server_in_background(
                &mut app_state,
                "server-cmd",
                SessionDimensions::default(),
            )
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "start_server_in_background blocked for {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn sanitize_private_key_normalizes_line_endings_and_trailing_newline() {
        let pasted = "-----BEGIN OPENSSH PRIVATE KEY-----\r\nabc\r\n-----END OPENSSH PRIVATE KEY-----";
        assert_eq!(
            sanitize_private_key(pasted),
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n"
        );
    }

    #[test]
    fn sanitize_private_key_leaves_clean_key_unchanged() {
        let clean = "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n";
        assert_eq!(sanitize_private_key(clean), clean);
    }

    #[test]
    fn sanitize_private_key_trims_surrounding_blank_lines() {
        let pasted = "\n\n  -----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n\n\n";
        assert_eq!(
            sanitize_private_key(pasted),
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n"
        );
    }

    fn ssh_test_connection() -> SSHConnection {
        SSHConnection {
            id: "ssh-1".to_string(),
            label: "Prod".to_string(),
            host: "example.com".to_string(),
            port: 2222,
            username: "deploy".to_string(),
            password: None,
            private_key: None,
        }
    }

    fn ssh_test_tab() -> SessionTab {
        SessionTab {
            id: "ssh-tab-1".to_string(),
            tab_type: TabType::Ssh,
            project_id: "project-1".to_string(),
            ssh_connection_id: Some("ssh-1".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn build_ssh_launch_spec_without_key_omits_identity_flag() {
        let state = AppState::default();

        let launch = build_ssh_launch_spec(&state, &ssh_test_tab(), &ssh_test_connection(), None);

        assert_eq!(launch.program, "ssh");
        assert_eq!(
            launch.args,
            vec![
                "deploy@example.com".to_string(),
                "-p".to_string(),
                "2222".to_string(),
            ]
        );
    }

    #[test]
    fn build_ssh_launch_spec_with_key_appends_identity_flag() {
        let state = AppState::default();
        let key_file = PathBuf::from("/keys/ssh-1");

        let launch = build_ssh_launch_spec(
            &state,
            &ssh_test_tab(),
            &ssh_test_connection(),
            Some(key_file.as_path()),
        );

        assert_eq!(
            launch.args,
            vec![
                "deploy@example.com".to_string(),
                "-p".to_string(),
                "2222".to_string(),
                "-i".to_string(),
                key_file.display().to_string(),
            ]
        );
    }

    #[test]
    fn safe_key_file_name_replaces_path_hostile_characters() {
        assert_eq!(safe_key_file_name("ssh-1a2b-3"), "ssh-1a2b-3");
        assert_eq!(safe_key_file_name("ssh/../evil"), "ssh____evil");
    }

    #[test]
    fn materialize_ssh_key_writes_sanitized_key_file() {
        let dir = temp_test_dir("materialize-ssh-key");
        let connection = SSHConnection {
            id: "ssh-test".to_string(),
            label: "Test".to_string(),
            host: "example.com".to_string(),
            port: 22,
            username: "deploy".to_string(),
            password: None,
            private_key: Some("-----BEGIN KEY-----\r\nabc\r\n-----END KEY-----".to_string()),
        };

        let path = materialize_ssh_key_in(&dir, &connection)
            .expect("materialize")
            .expect("path");

        assert_eq!(path, dir.join("ssh-test"));
        assert_eq!(
            fs::read_to_string(&path).expect("read key"),
            "-----BEGIN KEY-----\nabc\n-----END KEY-----\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dir_mode = fs::metadata(&dir).expect("dir metadata").permissions().mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }
    }

    #[test]
    fn materialize_ssh_key_rejects_empty_connection_id() {
        let dir = temp_test_dir("materialize-ssh-key-empty-id");
        let connection = SSHConnection {
            id: String::new(),
            label: "Test".to_string(),
            host: "example.com".to_string(),
            port: 22,
            username: "deploy".to_string(),
            password: None,
            private_key: Some("-----BEGIN KEY-----\nabc\n-----END KEY-----".to_string()),
        };

        let error = materialize_ssh_key_in(&dir, &connection).expect_err("should reject");
        assert!(error.contains("connection id"), "unexpected error: {error}");
    }

    #[test]
    fn sanitize_private_key_normalizes_lone_carriage_returns() {
        let input = "-----BEGIN KEY-----\rabc\r-----END KEY-----";
        assert_eq!(
            sanitize_private_key(input),
            "-----BEGIN KEY-----\nabc\n-----END KEY-----\n"
        );
    }

    #[test]
    fn materialize_ssh_key_returns_none_without_key_material() {
        let dir = temp_test_dir("materialize-ssh-key-empty");
        let connection = SSHConnection {
            id: "ssh-empty".to_string(),
            label: "Test".to_string(),
            host: "example.com".to_string(),
            port: 22,
            username: "deploy".to_string(),
            password: Some("pw".to_string()),
            private_key: Some("   \n".to_string()),
        };

        assert_eq!(materialize_ssh_key_in(&dir, &connection), Ok(None));
        assert!(!dir.join("ssh-empty").exists());
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

        let mut state = AppState::default();
        state.config = AppConfig {
            version: crate::models::CURRENT_CONFIG_VERSION,
            projects: vec![project],
            settings: Settings::default(),
            ssh_connections: Vec::new(),
        };
        state.mark_dirty();
        state
    }

    #[test]
    fn runtime_revision_tracks_semantic_changes_but_not_frame_metrics() {
        let manager = ProcessManager::new();
        let initial_revision = manager.runtime_revision();
        manager.register_runtime_session(SessionRuntimeState::new(
            "alpha",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        ));
        let after_register = manager.runtime_revision();
        assert!(after_register > initial_revision);

        let runtime_events = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let event_counter = runtime_events.clone();
        manager.set_remote_session_handler(Some(std::sync::Arc::new(move |event| {
            if matches!(event, RemoteSessionEvent::Runtime { .. }) {
                event_counter.fetch_add(1, Ordering::SeqCst);
            }
        })));
        runtime_events.store(0, Ordering::SeqCst);

        manager.record_frame("alpha", Duration::from_millis(4));
        assert_eq!(runtime_events.load(Ordering::SeqCst), 0);
        assert_eq!(manager.runtime_revision(), after_register);

        manager.set_active_session("alpha");
        let after_active = manager.runtime_revision();
        assert!(after_active > after_register);

        manager.set_active_session("alpha");
        assert_eq!(manager.runtime_revision(), after_active);
    }

    #[test]
    fn session_change_notifier_only_emits_when_dirty_generation_advances() {
        let manager = ProcessManager::new();
        manager.register_runtime_session(SessionRuntimeState::new(
            "alpha",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        ));
        let runtime_events = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let event_counter = runtime_events.clone();
        manager.set_remote_session_handler(Some(std::sync::Arc::new(move |event| {
            if matches!(event, RemoteSessionEvent::Runtime { .. }) {
                event_counter.fetch_add(1, Ordering::SeqCst);
            }
        })));
        runtime_events.store(0, Ordering::SeqCst);

        let notifier = session_change_notifier(manager.inner.clone(), "alpha".to_string());
        let initial_revision = manager.runtime_revision();
        notifier();
        assert_eq!(runtime_events.load(Ordering::SeqCst), 0);
        assert_eq!(manager.runtime_revision(), initial_revision);

        if let Ok(mut runtime) = manager.inner.runtime_state.write() {
            if let Some(session) = runtime.sessions.get_mut("alpha") {
                session.note_title(Some("ready".to_string()));
            }
        }

        notifier();
        let after_change = manager.runtime_revision();
        assert_eq!(runtime_events.load(Ordering::SeqCst), 1);
        assert!(after_change > initial_revision);

        notifier();
        assert_eq!(runtime_events.load(Ordering::SeqCst), 1);
        assert_eq!(manager.runtime_revision(), after_change);
    }

    #[test]
    fn record_frame_does_not_block_on_busy_runtime_lock() {
        let manager = ProcessManager::new();
        manager.register_runtime_session(SessionRuntimeState::new(
            "alpha",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        ));

        let runtime_guard = manager
            .inner
            .runtime_state
            .read()
            .expect("runtime read lock");
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = manager.clone();
        let handle = thread::spawn(move || {
            worker.record_frame("alpha", Duration::from_millis(1));
            tx.send(()).expect("record_frame completion");
        });

        let completed_while_locked = rx.recv_timeout(Duration::from_millis(50));
        drop(runtime_guard);
        handle.join().expect("record_frame thread joined");

        assert!(
            completed_while_locked.is_ok(),
            "record_frame blocked on runtime lock"
        );
    }

    #[test]
    fn kill_process_rejects_pid_outside_session_tree() {
        let cwd = temp_test_dir("kill-reject-foreign");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "shell-kill-reject";

        manager
            .spawn_shell_session(session_id, &cwd, SessionDimensions::default(), None, None)
            .unwrap();
        wait_for_live_session(&manager, session_id);

        let foreign_pid = 4_294_967_294;
        let completion = execute_process_op_inner(
            &manager.inner,
            ProcessOp::KillProcess {
                op_id: next_op_id(),
                session_id: session_id.to_string(),
                pid: foreign_pid,
                response: None,
            },
        );
        assert!(completion.result.is_err());
        assert!(completion
            .result
            .unwrap_err()
            .contains("not part of session"));

        let _ = manager.close_session(session_id);
    }

    #[test]
    fn kill_process_rejects_stale_resource_pid_without_verified_identity() {
        let cwd = temp_test_dir("kill-reject-stale");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "stale-kill-session";
        let running_pid = std::process::id();

        {
            let mut runtime = manager.inner.runtime_state.write().expect("runtime write");
            let mut session = SessionRuntimeState::new(
                session_id,
                cwd.clone(),
                SessionDimensions::default(),
                TerminalBackend::PortablePtyFeedingAlacritty,
            );
            session.status = SessionStatus::Failed;
            session.reap_incomplete = true;
            session.pid = None;
            session.resources = ResourceSnapshot {
                process_count: 1,
                process_ids: vec![running_pid],
                processes: vec![crate::state::ProcessResourceNode {
                    pid: running_pid,
                    parent_pid: None,
                    name: "stale".to_string(),
                    cpu_percent: 0.0,
                    memory_bytes: 0,
                }],
                ..Default::default()
            };
            runtime.sessions.insert(session_id.to_string(), session);
        }

        let completion = execute_process_op_inner(
            &manager.inner,
            ProcessOp::KillProcess {
                op_id: next_op_id(),
                session_id: session_id.to_string(),
                pid: running_pid,
                response: None,
            },
        );
        assert!(completion.result.is_err());
        assert!(completion
            .result
            .unwrap_err()
            .contains("not part of session"));
    }

    #[test]
    fn kill_process_accepts_verified_live_session_root() {
        let cwd = temp_test_dir("kill-accept-root");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "shell-kill-accept";

        manager
            .spawn_shell_session(session_id, &cwd, SessionDimensions::default(), None, None)
            .unwrap();
        wait_for_live_session(&manager, session_id);
        let pid = manager
            .runtime_state()
            .sessions
            .get(session_id)
            .and_then(|session| session.pid)
            .expect("live pid");

        let completion = execute_process_op_inner(
            &manager.inner,
            ProcessOp::KillProcess {
                op_id: next_op_id(),
                session_id: session_id.to_string(),
                pid,
                response: None,
            },
        );
        assert!(completion.result.is_ok(), "{:?}", completion.result);
        assert!(
            completion
                .context
                .message
                .as_deref()
                .is_some_and(|message| message.contains(&format!("Killed process {pid}"))),
            "unexpected message: {:?}",
            completion.context.message
        );
    }

    #[test]
    fn note_reap_incomplete_marks_failed_session_with_tracked_pids() {
        let cwd = temp_test_dir("reap-incomplete");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "reap-incomplete-session";

        let identity =
            platform_service::capture_process_identity(std::process::id()).expect("self identity");
        pid_file::track_session_process(pid_file::ManagedProcessRecord {
            session_id: session_id.to_string(),
            pid: identity.pid,
            started_at_unix_secs: identity.started_at_unix_secs,
            process_name: identity.process_name.clone(),
            session_kind: "shell".to_string(),
            program: "test-shell".to_string(),
            project_id: None,
            command_id: None,
            tab_id: None,
            descendant_processes: Vec::new(),
        })
        .unwrap();

        {
            let mut runtime = manager.inner.runtime_state.write().expect("runtime write");
            let mut session = SessionRuntimeState::new(
                session_id,
                cwd.clone(),
                SessionDimensions::default(),
                TerminalBackend::PortablePtyFeedingAlacritty,
            );
            session.status = SessionStatus::Stopping;
            session.pid = Some(identity.pid);
            runtime.sessions.insert(session_id.to_string(), session);
        }

        manager.note_reap_incomplete(session_id);
        let runtime = manager.runtime_state();
        let session = runtime.sessions.get(session_id).expect("session");
        assert!(session.reap_incomplete);
        assert_eq!(session.status, SessionStatus::Failed);
        assert!(session.pid.is_none());
        assert!(session.resources.process_ids.contains(&identity.pid));
        assert!(session
            .exit
            .as_ref()
            .is_some_and(|exit| exit.summary.contains("tracked process")));
    }

    #[test]
    fn refresh_resource_snapshots_populates_named_process_nodes() {
        let cwd = temp_test_dir("resource-sample-nodes");
        let _pid_file_guard = pid_file::use_test_pid_file(cwd.join("running-pids.json"));
        let manager = ProcessManager::new();
        let session_id = "shell-sample-nodes";

        manager
            .spawn_shell_session(session_id, &cwd, SessionDimensions::default(), None, None)
            .unwrap();
        wait_for_live_session(&manager, session_id);
        wait_for_tracked_process(session_id);

        let mut system = sysinfo::System::new();
        refresh_resource_snapshots(&manager.inner, &mut system);

        let session = manager
            .runtime_state()
            .sessions
            .get(session_id)
            .cloned()
            .expect("session");
        assert!(
            !session.resources.processes.is_empty(),
            "expected named process nodes from sampler"
        );
        assert_eq!(
            session.resources.process_count as usize,
            session.resources.processes.len()
        );
        assert!(!session.resources.processes[0].name.is_empty());

        let _ = manager.close_session(session_id);
    }

    #[test]
    fn resource_snapshot_processes_round_trip_in_session_state() {
        let mut session = SessionRuntimeState::new(
            "resource-nodes",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.note_resource_sample(ResourceSnapshot {
            cpu_percent: 12.5,
            memory_bytes: 2048,
            process_count: 2,
            process_ids: vec![1, 2],
            processes: vec![
                crate::state::ProcessResourceNode {
                    pid: 1,
                    parent_pid: None,
                    name: "shell".to_string(),
                    cpu_percent: 1.0,
                    memory_bytes: 1024,
                },
                crate::state::ProcessResourceNode {
                    pid: 2,
                    parent_pid: Some(1),
                    name: "node".to_string(),
                    cpu_percent: 11.5,
                    memory_bytes: 1024,
                },
            ],
            last_sample_at: Some(Instant::now()),
        });
        assert_eq!(session.resources.processes.len(), 2);
        assert_eq!(session.resources.processes[1].name, "node");
    }

    fn wait_for_live_session(manager: &ProcessManager, session_id: &str) {
        for _ in 0..50 {
            let _ = manager.drain_process_op_completions();
            if manager
                .runtime_state()
                .sessions
                .get(session_id)
                .map(|session| session.status.is_live())
                .unwrap_or(false)
                && manager.get_session(session_id).is_ok()
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
