mod chrome;

use crate::assets::AppAssets;
use crate::git::git_service;
use crate::models::{
    AppConfig, DependencyStatus, MacTerminalProfile, PortStatus, Project, ProjectFolder,
    RunCommand, SSHConnection, SessionState, SessionTab, TabType,
};
use crate::notifications;
use crate::remote::{
    self, ClientAuth, LocalPortForwardManager, PendingRemoteRequest, RemoteAction,
    RemoteActionPayload, RemoteActionResult, RemoteClientHandle, RemoteClientPool, RemoteGitRepo,
    RemoteHostService, RemoteLatencyStats, RemoteMachineState, RemotePortForwardState,
    RemoteSessionBootstrap, RemoteTerminalExport, RemoteTerminalInput,
};
use crate::services::{
    env_service, pid_file, platform_service, ports_service, scanner_service, ConfigImportMode,
    ManagedShutdownReport, ProcessManager, RemoteSessionEvent, SessionManager,
};
use crate::sidebar;
use crate::state::{AppState, RuntimeState, SessionDimensions, SessionRuntimeState, SessionStatus};
use crate::terminal::{self, view};
use crate::theme;
use crate::updater::UpdaterService;
use crate::workspace::{
    self, CommandDraft, EditorAction, EditorField, EditorPaneModel, EditorPanel, FolderDraft,
    FolderField, ProjectDraft, RemotePortForwardDraft, SettingsDraft, SshDraft, UiPreviewDraft,
};
use gpui::{
    div, prelude::*, px, rgb, size, App, AppContext, Application, Bounds, ClipboardEntry,
    ClipboardItem, Context, FocusHandle, IntoElement, KeyDownEvent, Keystroke, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point,
    Render, RenderImage, ScrollWheelEvent, Styled, Subscription, TouchPhase, Window, WindowBounds,
    WindowOptions,
};
use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TERMINAL_TOPBAR_HEIGHT_PX: f32 = 22.0;
const STACK_GAP_PX: f32 = 4.0;
const META_TEXT_HEIGHT_PX: f32 = 0.0;
const NOTICE_HEIGHT_PX: f32 = 26.0;
const SEARCH_BAR_HEIGHT_PX: f32 = 34.0;
const FOOTER_HEIGHT_PX: f32 = 0.0;
const APP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const REMOTE_CLIENT_REFRESH_INTERVAL: Duration = Duration::from_millis(16);
const REMOTE_HOST_SNAPSHOT_ACTIVE_INTERVAL: Duration = Duration::from_millis(33);
const REMOTE_HOST_SNAPSHOT_IDLE_INTERVAL: Duration = Duration::from_millis(250);
const REMOTE_RECONNECT_BASE_INTERVAL: Duration = Duration::from_millis(350);
const REMOTE_RECONNECT_MAX_INTERVAL: Duration = Duration::from_secs(5);
const APP_WINDOW_TITLE: &str = "DevManager";
const WINDOW_TITLE_SEPARATOR: &str = " • ";

static EDITOR_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

pub fn run() {
    Application::new()
        .with_assets(AppAssets::new())
        .run(|cx: &mut App| {
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let saved_bounds = SessionManager::new()
                .load_workspace()
                .ok()
                .and_then(|snapshot| snapshot.session.window_bounds);

            let window_bounds = if let Some(wb) = saved_bounds {
                let bounds = Bounds {
                    origin: Point::new(px(wb.x), px(wb.y)),
                    size: size(px(wb.width.max(400.0)), px(wb.height.max(300.0))),
                };
                if wb.maximized {
                    WindowBounds::Maximized(bounds)
                } else {
                    WindowBounds::Windowed(bounds)
                }
            } else {
                WindowBounds::Windowed(Bounds::centered(None, size(px(1440.0), px(920.0)), cx))
            };

            cx.open_window(
                WindowOptions {
                    window_bounds: Some(window_bounds),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some(app_window_title().into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let shell = cx.new(NativeShell::new);
                    let close_handler = shell.clone();
                    window.on_window_should_close(cx, move |window, cx| {
                        close_handler
                            .update(cx, |shell, cx| shell.handle_window_should_close(window, cx))
                    });
                    let _ = shell.update(cx, |shell, cx| {
                        shell.register_focus_observers(window, cx);
                    });
                    shell
                },
            )
            .unwrap();
            cx.activate(true);
        });
}

struct NativeShell {
    state: AppState,
    session_manager: SessionManager,
    process_manager: ProcessManager,
    updater: UpdaterService,
    startup_notice: Option<String>,
    terminal_notice: Option<String>,
    editor_notice: Option<String>,
    remote_machine_state: RemoteMachineState,
    remote_host_service: RemoteHostService,
    remote_client_pool: RemoteClientPool,
    remote_mode: Option<RemoteModeState>,
    local_state_backup: Option<AppState>,
    last_remote_host_config_revision: u64,
    last_remote_snapshot_sync_at: Option<Instant>,
    last_remote_runtime_revision: u64,
    last_remote_port_hash: u64,
    remote_live_session_generations: HashMap<String, u64>,
    local_viewer_replicas: HashMap<String, LocalViewerReplicaState>,
    terminal_focus: FocusHandle,
    editor_focus: FocusHandle,
    did_focus_terminal: bool,
    focused_terminal_session_id: Option<String>,
    active_port_state: Option<ActivePortState>,
    server_port_snapshot: ServerPortSnapshotState,
    ssh_password_prompt_state: Option<SshPasswordPromptState>,
    editor_needs_focus: bool,
    synced_session_id: Option<String>,
    last_dimensions: Option<SessionDimensions>,
    terminal_selection: Option<TerminalSelection>,
    terminal_scroll_px: Pixels,
    is_selecting_terminal: bool,
    last_terminal_mouse_report: Option<(TerminalGridPosition, Option<MouseButton>)>,
    terminal_scrollbar_drag: Option<TerminalScrollbarDrag>,
    pending_terminal_display_offset: Option<usize>,
    terminal_search: TerminalSearchState,
    editor_panel: Option<EditorPanel>,
    editor_active_field: Option<EditorField>,
    editor_cursor: usize,
    editor_selection_anchor: Option<usize>,
    is_selecting_editor: bool,
    sidebar_context_menu: Option<sidebar::SidebarContextMenu>,
    add_project_wizard: Option<workspace::AddProjectWizard>,
    last_window_title: Option<String>,
    splash_image: Option<Arc<RenderImage>>,
    splash_fetch_in_flight: bool,
    native_dialog_blockers: Arc<AtomicUsize>,
    remote_connect_request_id: u64,
    remote_status_notice: Option<RemoteStatusNotice>,
    window_subscriptions: Vec<Subscription>,
}

struct NativeDialogPauseGuard {
    blockers: Arc<AtomicUsize>,
}

struct PreparedRemoteConnect {
    address: String,
    port: u16,
    host_label: String,
    auth: ClientAuth,
    expected_fingerprint: Option<String>,
    known_server_id: Option<String>,
}

#[derive(Debug, Clone)]
struct RemoteStatusNotice {
    message: String,
    is_error: bool,
}

#[derive(Debug, Clone, Copy)]
enum RemoteStatusBarAction {
    ConnectPreferred,
    RetryReconnect,
    DisconnectRemote,
    TakeRemoteControl,
    ReleaseRemoteControl,
    TakeHostControl,
    CopyPairToken,
    OpenRemoteSettings,
}

struct RemoteStatusBarState {
    model: chrome::RemoteStatusBarModel,
    primary_action: Option<RemoteStatusBarAction>,
    secondary_action: Option<RemoteStatusBarAction>,
    tertiary_action: Option<RemoteStatusBarAction>,
}

impl Drop for NativeDialogPauseGuard {
    fn drop(&mut self) {
        self.blockers.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TerminalGridPosition {
    row: usize,
    column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TerminalCellSide {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TerminalSelectionEndpoint {
    position: TerminalGridPosition,
    side: TerminalCellSide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalSelectionMode {
    Simple,
    Semantic,
    Lines,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalSelection {
    anchor: TerminalSelectionEndpoint,
    head: TerminalSelectionEndpoint,
    moved: bool,
    mode: TerminalSelectionMode,
}

#[derive(Debug, Clone, Copy)]
struct TerminalTextBounds {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    cell_width: f32,
    row_height: f32,
    rows: usize,
    cols: usize,
}

#[derive(Debug, Clone, Copy)]
struct TerminalViewportLayout {
    left: f32,
    top: f32,
    available_width: f32,
    available_height: f32,
}

#[derive(Debug, Clone, Copy)]
struct TerminalRenderMetrics {
    cell_width: f32,
    line_height: f32,
}

#[derive(Debug, Clone, Copy)]
struct TerminalScrollbarGeometry {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    track_top: f32,
    track_height: f32,
    thumb_top: f32,
    thumb_height: f32,
    max_offset: usize,
}

#[derive(Debug, Clone, Copy)]
struct TerminalScrollbarDrag {
    grab_offset_px: f32,
    thumb_top_ratio: f32,
    last_display_offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortKillFeedback {
    Killed,
    None,
    Error,
}

#[derive(Debug, Clone)]
struct ActivePortState {
    command_id: String,
    port: u16,
    status: Option<PortStatus>,
    last_checked_at: Option<Instant>,
    kill_feedback: Option<PortKillFeedback>,
    kill_feedback_until: Option<Instant>,
    refresh_in_flight: bool,
}

#[derive(Debug, Clone, Default)]
struct ServerPortSnapshotState {
    tracked_ports: Vec<u16>,
    statuses: HashMap<u16, PortStatus>,
    last_checked_at: Option<Instant>,
    refresh_in_flight: bool,
}

#[derive(Debug, Clone, Default)]
struct TerminalSearchState {
    active: bool,
    query: String,
    matches: Vec<crate::terminal::session::TerminalSearchMatch>,
    selected_index: Option<usize>,
    case_sensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SshPasswordPromptState {
    session_id: String,
    fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SshPasswordPromptMatch {
    fingerprint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalSelectionRange {
    start_row: usize,
    start_column: usize,
    end_row: usize,
    end_column: usize,
}

#[derive(Clone)]
struct RemoteModeState {
    client: RemoteClientHandle,
    port_forwards: LocalPortForwardManager,
    snapshot: remote::RemoteWorkspaceSnapshot,
    connected_label: String,
    address: String,
    port: u16,
    pool_key: String,
    subscribed_session_ids: BTreeSet<String>,
    last_snapshot_revision: u64,
    last_session_stream_revision: u64,
    reconnect: Option<RemoteReconnectState>,
}

#[derive(Clone)]
struct RemoteReconnectState {
    attempts: u32,
    next_attempt_at: Instant,
    in_flight: bool,
    last_disconnect_message: Option<String>,
    last_error: Option<String>,
}

struct LocalViewerReplicaState {
    dirty_generation: u64,
    replica: crate::terminal::session::TerminalReplica,
}

fn format_remote_latency_summary(stats: &RemoteLatencyStats) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(ms) = stats.input_enqueue_to_host_write_ms {
        parts.push(format!("write {ms} ms"));
    }
    if let Some(ms) = stats.output_host_to_client_ms {
        parts.push(format!("host {ms} ms"));
    }
    if let Some(ms) = stats.output_client_to_paint_ms {
        parts.push(format!("paint {ms} ms"));
    }
    (!parts.is_empty()).then(|| parts.join(" • "))
}

fn remote_role_label(has_control: bool) -> &'static str {
    if has_control {
        "Controller"
    } else {
        "Viewer"
    }
}

impl NativeShell {
    fn new(cx: &mut Context<Self>) -> Self {
        let session_manager = SessionManager::new();
        let remote_machine_state = remote::load_remote_machine_state().unwrap_or_default();
        let native_dialog_blockers = Arc::new(AtomicUsize::new(0));
        let (mut state, startup_notice) = match session_manager.load_workspace() {
            Ok(snapshot) => (AppState::from_workspace(snapshot), None),
            Err(error) => (
                AppState::default(),
                Some(format!(
                    "Fell back to an empty workspace because legacy state could not be loaded: {error}"
                )),
            ),
        };
        let process_manager = ProcessManager::new();
        process_manager.set_settings(state.config.settings.clone());
        process_manager.set_notification_sound(state.config.settings.notification_sound.clone());
        process_manager.set_log_buffer_size(state.config.settings.log_buffer_size as usize);
        let updater = UpdaterService::new();
        let remote_host_service = RemoteHostService::new(remote_machine_state.host.clone());
        let bootstrap_manager = process_manager.clone();
        remote_host_service.set_session_bootstrap_provider(Some(Arc::new(move |session_id| {
            let session_view = bootstrap_manager.session_view(session_id)?;
            let replay_bytes = bootstrap_manager.session_replay_bytes(session_id).ok()?;
            Some(RemoteSessionBootstrap {
                session_id: session_id.to_string(),
                runtime: session_view.runtime,
                screen: session_view.screen,
                replay_bytes,
            })
        })));
        let input_manager = process_manager.clone();
        let input_host_service = remote_host_service.clone();
        remote_host_service.set_terminal_input_handler(Some(Arc::new(
            move |input, enqueued_at_epoch_ms| {
                let result = match input {
                    RemoteTerminalInput::Text { session_id, text } => {
                        input_manager.write_to_session(&session_id, &text)
                    }
                    RemoteTerminalInput::Bytes { session_id, bytes } => {
                        input_manager.write_bytes_to_session(&session_id, &bytes)
                    }
                    RemoteTerminalInput::Paste { session_id, text } => {
                        input_manager.paste_to_session(&session_id, &text)
                    }
                };
                if result.is_ok() {
                    input_host_service.record_input_write_latency(enqueued_at_epoch_ms);
                }
            },
        )));
        let resize_manager = process_manager.clone();
        remote_host_service.set_terminal_resize_handler(Some(Arc::new(
            move |session_id, dimensions| {
                let _ = resize_manager.resize_session(&session_id, dimensions);
            },
        )));
        let focus_manager = process_manager.clone();
        remote_host_service.set_focused_session_handler(Some(Arc::new(move |session_id| {
            focus_manager.set_active_session(session_id);
        })));
        let event_host_service = remote_host_service.clone();
        process_manager.set_remote_session_handler(Some(Arc::new(move |event| match event {
            RemoteSessionEvent::Output { session_id, bytes } => {
                event_host_service.push_session_output(&session_id, bytes);
            }
            RemoteSessionEvent::Runtime {
                session_id,
                runtime,
            } => {
                event_host_service.push_session_runtime(&session_id, runtime);
            }
            RemoteSessionEvent::Removed { session_id } => {
                event_host_service.push_session_removed(&session_id);
            }
        })));
        let remote_client_pool = RemoteClientPool::default();
        let mut terminal_notice = None;
        let restore_enabled = state
            .config
            .settings
            .restore_session_on_start
            .unwrap_or(true);

        pid_file::cleanup_orphaned_processes();

        if !restore_enabled {
            state.open_tabs.clear();
            state.active_tab_id = None;
            state.sidebar_collapsed = false;
        } else {
            terminal_notice =
                restore_saved_tabs(&process_manager, &mut state, SessionDimensions::default());
        }

        // Start with no terminal loaded — the user picks a tab from the sidebar.
        state.active_tab_id = None;
        let synced_session_id: Option<String> = None;

        let _ = session_manager.save_session(&persisted_session_state(&state));
        updater.start_background_checks();
        if updater.is_configured() {
            Self::spawn_updater_refresh_task(updater.clone(), native_dialog_blockers.clone(), cx);
        }
        Self::spawn_remote_refresh_task(native_dialog_blockers.clone(), cx);

        let shell = Self {
            state,
            session_manager,
            process_manager,
            updater,
            startup_notice,
            terminal_notice,
            editor_notice: None,
            remote_machine_state,
            remote_host_service,
            remote_client_pool,
            remote_mode: None,
            local_state_backup: None,
            last_remote_host_config_revision: 0,
            last_remote_snapshot_sync_at: None,
            last_remote_runtime_revision: 0,
            last_remote_port_hash: 0,
            remote_live_session_generations: HashMap::new(),
            local_viewer_replicas: HashMap::new(),
            terminal_focus: cx.focus_handle(),
            editor_focus: cx.focus_handle(),
            did_focus_terminal: false,
            focused_terminal_session_id: None,
            active_port_state: None,
            server_port_snapshot: ServerPortSnapshotState::default(),
            ssh_password_prompt_state: None,
            editor_needs_focus: false,
            synced_session_id,
            last_dimensions: None,
            terminal_selection: None,
            terminal_scroll_px: px(0.0),
            is_selecting_terminal: false,
            last_terminal_mouse_report: None,
            terminal_scrollbar_drag: None,
            pending_terminal_display_offset: None,
            terminal_search: TerminalSearchState::default(),
            editor_panel: None,
            editor_active_field: None,
            editor_cursor: 0,
            editor_selection_anchor: None,
            is_selecting_editor: false,
            sidebar_context_menu: None,
            add_project_wizard: None,
            last_window_title: None,
            splash_image: None,
            splash_fetch_in_flight: false,
            native_dialog_blockers,
            remote_connect_request_id: 0,
            remote_status_notice: None,
            window_subscriptions: Vec::new(),
        };

        Self::spawn_splash_image_fetch(shell.native_dialog_blockers.clone(), cx);

        shell
    }

    fn register_focus_observers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.window_subscriptions.is_empty() {
            return;
        }

        self.window_subscriptions
            .push(cx.observe_window_activation(window, Self::handle_window_activation_changed));
        self.window_subscriptions.push(cx.on_focus_in(
            &self.terminal_focus,
            window,
            Self::handle_terminal_focus_in,
        ));
        self.window_subscriptions.push(cx.on_focus_out(
            &self.terminal_focus,
            window,
            Self::handle_terminal_focus_out,
        ));
    }

    fn handle_window_activation_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = window.is_window_active();

        let session_id = self.terminal_focus_session_id();

        if let Some(session_id) = session_id {
            let _ = self.process_manager.report_focus(&session_id, active);
        }

        if active {
            if self.editor_panel.is_none() {
                self.did_focus_terminal = false;
            }
            if let Some(remote_mode) = self.remote_mode.as_mut() {
                if let Some(reconnect) = remote_mode.reconnect.as_mut() {
                    reconnect.next_attempt_at = Instant::now();
                    reconnect.in_flight = false;
                    self.try_begin_remote_reconnect(cx);
                }
            }
        } else {
            self.last_terminal_mouse_report = None;
            self.is_selecting_terminal = false;
        }

        cx.notify();
    }

    fn handle_terminal_focus_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session_id) = self.terminal_focus_session_id() {
            let _ = self.process_manager.report_focus(&session_id, true);
        }
        self.did_focus_terminal = true;
        window.invalidate_character_coordinates();
        cx.notify();
    }

    fn handle_terminal_focus_out(
        &mut self,
        _event: gpui::FocusOutEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session_id) = self.terminal_focus_session_id() {
            let _ = self.process_manager.report_focus(&session_id, false);
        }
        self.did_focus_terminal = false;
        self.last_terminal_mouse_report = None;
        self.is_selecting_terminal = false;
        cx.notify();
    }

    fn terminal_focus_session_id(&self) -> Option<String> {
        self.focused_terminal_session_id.clone().or_else(|| {
            if self.editor_panel.is_none() {
                self.state
                    .active_tab()
                    .and_then(|tab| tab.pty_session_id.clone())
            } else {
                None
            }
        })
    }

    fn spawn_splash_image_fetch(native_dialog_blockers: Arc<AtomicUsize>, cx: &mut Context<Self>) {
        let executor = cx.background_executor().clone();
        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let mut async_cx = cx.clone();
                let native_dialog_blockers = native_dialog_blockers.clone();
                async move {
                    let image = executor.spawn(async move { fetch_splash_image() }).await;
                    while native_dialog_blockers.load(Ordering::Acquire) > 0 {
                        executor.timer(Duration::from_millis(50)).await;
                    }
                    let _ = this.update(&mut async_cx, |shell, cx| {
                        shell.splash_fetch_in_flight = false;
                        if let Some(image) = image {
                            shell.splash_image = Some(image);
                        }
                        cx.notify();
                    });
                }
            },
        )
        .detach();
    }

    fn ensure_splash_image(&mut self, cx: &mut Context<Self>) {
        if self.splash_image.is_none() && !self.splash_fetch_in_flight {
            self.splash_fetch_in_flight = true;
            Self::spawn_splash_image_fetch(self.native_dialog_blockers.clone(), cx);
        }
    }

    fn spawn_updater_refresh_task(
        updater: UpdaterService,
        native_dialog_blockers: Arc<AtomicUsize>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let background_executor = cx.background_executor().clone();
                let mut async_cx = cx.clone();
                let native_dialog_blockers = native_dialog_blockers.clone();
                async move {
                    let mut previous_snapshot = updater.snapshot();
                    loop {
                        background_executor.timer(Duration::from_millis(500)).await;
                        let next_snapshot = updater.snapshot();
                        if next_snapshot != previous_snapshot {
                            previous_snapshot = next_snapshot;
                            while native_dialog_blockers.load(Ordering::Acquire) > 0 {
                                background_executor.timer(Duration::from_millis(50)).await;
                            }
                            if this
                                .update(&mut async_cx, |_, cx: &mut Context<'_, Self>| cx.notify())
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            },
        )
        .detach();
    }

    fn spawn_remote_refresh_task(native_dialog_blockers: Arc<AtomicUsize>, cx: &mut Context<Self>) {
        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let background_executor = cx.background_executor().clone();
                let mut async_cx = cx.clone();
                let native_dialog_blockers = native_dialog_blockers.clone();
                async move {
                    loop {
                        background_executor
                            .timer(REMOTE_CLIENT_REFRESH_INTERVAL)
                            .await;
                        while native_dialog_blockers.load(Ordering::Acquire) > 0 {
                            background_executor.timer(Duration::from_millis(50)).await;
                        }
                        if this
                            .update(&mut async_cx, |shell, cx: &mut Context<'_, Self>| {
                                let changed = if shell.remote_mode.is_some() {
                                    shell.sync_remote_client_snapshot(cx)
                                } else {
                                    let changed = shell.pump_remote_host_requests(cx);
                                    let local_runtime_snapshot =
                                        shell.process_manager.runtime_state();
                                    shell.sync_server_port_snapshot(&local_runtime_snapshot, cx);
                                    shell.sync_remote_host_live_sessions(&local_runtime_snapshot);
                                    shell.sync_remote_host_snapshot_if_due(&local_runtime_snapshot);
                                    changed
                                };
                                if changed {
                                    cx.notify();
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            },
        )
        .detach();
    }

    fn pause_for_native_dialog(&self) -> NativeDialogPauseGuard {
        self.native_dialog_blockers.fetch_add(1, Ordering::AcqRel);
        NativeDialogPauseGuard {
            blockers: self.native_dialog_blockers.clone(),
        }
    }

    fn save_session_state(&mut self) {
        if self.remote_mode.is_some() {
            return;
        }
        if let Err(error) = self
            .session_manager
            .save_session(&persisted_session_state(&self.state))
        {
            self.terminal_notice = Some(format!("Failed to save session state: {error}"));
        }
    }

    fn save_config_state(&mut self) {
        if self.remote_mode.is_some() {
            return;
        }
        if let Err(error) = self.session_manager.save_config(&self.state.config) {
            self.editor_notice = Some(format!("Failed to save config: {error}"));
        } else {
            self.process_manager
                .set_settings(self.state.config.settings.clone());
            self.process_manager
                .set_notification_sound(self.state.config.settings.notification_sound.clone());
            self.process_manager
                .set_log_buffer_size(self.state.config.settings.log_buffer_size as usize);
        }
    }

    fn persist_remote_machine_state(&mut self) {
        if let Err(error) = remote::save_remote_machine_state(&self.remote_machine_state) {
            self.editor_notice = Some(format!("Failed to save remote settings: {error}"));
        }
    }

    fn sync_remote_host_config_from_service(&mut self) {
        let latest_revision = self.remote_host_service.config_revision();
        if latest_revision == self.last_remote_host_config_revision {
            return;
        }
        let latest = self.remote_host_service.config();
        if self.remote_machine_state.host != latest {
            self.remote_machine_state.host = latest;
            self.persist_remote_machine_state();
            self.sync_settings_remote_draft();
        }
        self.last_remote_host_config_revision = latest_revision;
    }

    fn sync_remote_client_snapshot(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(client) = self
            .remote_mode
            .as_ref()
            .map(|remote_mode| remote_mode.client.clone())
        else {
            return false;
        };

        if self
            .remote_mode
            .as_ref()
            .and_then(|remote_mode| remote_mode.reconnect.as_ref())
            .is_some()
        {
            let changed = self.try_begin_remote_reconnect(cx);
            self.sync_settings_remote_draft();
            return changed;
        }

        if let Some(message) = client.disconnected_message() {
            self.begin_remote_reconnect(message, cx);
            return true;
        }

        let (last_snapshot_revision, last_session_stream_revision) = self
            .remote_mode
            .as_ref()
            .map(|remote_mode| {
                (
                    remote_mode.last_snapshot_revision,
                    remote_mode.last_session_stream_revision,
                )
            })
            .unwrap_or((0, 0));

        let mut changed = false;
        let mut latest_snapshot = None;
        let snapshot_revision = client.snapshot_revision();
        if snapshot_revision != last_snapshot_revision {
            let Some(snapshot) = client.latest_snapshot() else {
                return false;
            };
            self.state = self.merge_remote_snapshot_into_state(&snapshot);
            latest_snapshot = Some(snapshot);
            changed = true;
        }

        let session_stream_revision = client.session_stream_revision();
        if session_stream_revision != last_session_stream_revision {
            changed = true;
        }

        if let Some(remote_mode) = self.remote_mode.as_mut() {
            if let Some(snapshot) = latest_snapshot {
                remote_mode.snapshot = snapshot;
                remote_mode.last_snapshot_revision = snapshot_revision;
            }
            if session_stream_revision != remote_mode.last_session_stream_revision {
                remote_mode.last_session_stream_revision = session_stream_revision;
            }
        }

        if client.drain_pending_notifications() > 0 {
            let sound_id = self.state.config.settings.notification_sound.as_deref();
            notifications::play_notification_sound(sound_id);
        }

        let forward_changed = self.sync_remote_port_forwards();
        if changed || forward_changed {
            self.sync_remote_session_subscriptions();
            self.sync_settings_remote_draft();
        }

        changed || forward_changed
    }

    fn desired_remote_session_subscriptions(&self) -> BTreeSet<String> {
        self.state
            .open_tabs
            .iter()
            .filter_map(|tab| match tab.tab_type {
                TabType::Server => tab.command_id.clone(),
                TabType::Claude | TabType::Codex | TabType::Ssh => tab
                    .pty_session_id
                    .clone()
                    .or_else(|| tab.command_id.clone()),
            })
            .collect()
    }

    fn sync_remote_session_subscriptions(&mut self) {
        let desired = self.desired_remote_session_subscriptions();
        let Some(remote_mode) = self.remote_mode.as_mut() else {
            return;
        };

        let to_subscribe = desired
            .difference(&remote_mode.subscribed_session_ids)
            .cloned()
            .collect::<Vec<_>>();
        let to_unsubscribe = remote_mode
            .subscribed_session_ids
            .difference(&desired)
            .cloned()
            .collect::<Vec<_>>();

        if !to_subscribe.is_empty() {
            remote_mode.client.subscribe_sessions(to_subscribe);
        }
        if !to_unsubscribe.is_empty() {
            remote_mode.client.unsubscribe_sessions(to_unsubscribe);
        }

        remote_mode.subscribed_session_ids = desired;
    }

    fn merge_remote_snapshot_into_state(
        &self,
        snapshot: &remote::RemoteWorkspaceSnapshot,
    ) -> AppState {
        let preserve_active = self.state.active_tab_id.clone();
        let preserve_sidebar = self.state.sidebar_collapsed;
        let preserve_collapsed = self.state.collapsed_projects.clone();
        let mut next = snapshot.app_state.clone();

        next.active_tab_id = preserve_active
            .filter(|active| next.open_tabs.iter().any(|tab| &tab.id == active))
            .or_else(|| next.open_tabs.first().map(|tab| tab.id.clone()));
        next.sidebar_collapsed = preserve_sidebar;
        next.collapsed_projects = preserve_collapsed
            .into_iter()
            .filter(|project_id| {
                next.projects()
                    .iter()
                    .any(|project| &project.id == project_id)
            })
            .collect();
        next.window_bounds = self.state.window_bounds;
        next
    }

    fn remote_has_control(&self) -> bool {
        self.remote_mode
            .as_ref()
            .map(|remote_mode| remote_mode.snapshot.you_have_control)
            .unwrap_or(false)
    }

    fn local_host_has_control(&self) -> bool {
        self.remote_host_service.local_has_control()
    }

    fn ensure_remote_control(&mut self, cx: &mut Context<Self>) -> bool {
        if self.remote_mode.is_none() || self.remote_has_control() {
            if self
                .remote_mode
                .as_ref()
                .and_then(|remote_mode| remote_mode.reconnect.as_ref())
                .is_none()
            {
                return true;
            }
        }
        if self
            .remote_mode
            .as_ref()
            .and_then(|remote_mode| remote_mode.reconnect.as_ref())
            .is_some()
        {
            self.editor_notice = Some("Reconnecting to remote host...".to_string());
            cx.notify();
            return false;
        }
        self.editor_notice =
            Some("This remote client is in viewer mode. Take control first.".to_string());
        cx.notify();
        false
    }

    fn ensure_mutation_control(&mut self, cx: &mut Context<Self>) -> bool {
        if self.remote_mode.is_some() {
            return self.ensure_remote_control(cx);
        }
        if self.local_host_has_control() {
            return true;
        }
        self.remote_host_service.take_local_control();
        self.editor_notice =
            Some("Took control back from the connected remote client.".to_string());
        self.sync_settings_remote_draft();
        cx.notify();
        true
    }

    fn terminal_input_block_reason(&self) -> Option<String> {
        if let Some(remote_mode) = self.remote_mode.as_ref() {
            if remote_mode.reconnect.is_some() {
                return Some(
                    "Reconnecting to the remote host. Input will resume automatically.".to_string(),
                );
            }
            if !remote_mode.snapshot.you_have_control {
                return Some(
                    "Viewer mode is active. Take control to type or send terminal input."
                        .to_string(),
                );
            }
            return None;
        }

        if !self.local_host_has_control() {
            return Some(
                "Another remote client controls this host. Take local control to type here."
                    .to_string(),
            );
        }

        None
    }

    fn remote_request(&mut self, action: RemoteAction) -> Result<RemoteActionResult, String> {
        let Some(remote_mode) = self.remote_mode.as_ref() else {
            return Err("Remote host is not connected.".to_string());
        };
        if remote_mode.reconnect.is_some() {
            return Err("Reconnecting to remote host...".to_string());
        }
        remote_mode.client.request(action)
    }

    fn remote_send_terminal_input(&mut self, input: RemoteTerminalInput) {
        if let Some(remote_mode) = self.remote_mode.as_ref() {
            if remote_mode.reconnect.is_some() {
                return;
            }
            remote_mode.client.send_terminal_input(input);
        }
    }

    fn remote_send_action(&mut self, action: RemoteAction) {
        if let Some(remote_mode) = self.remote_mode.as_ref() {
            if remote_mode.reconnect.is_some() {
                return;
            }
            remote_mode.client.send_action(action);
        }
    }

    fn apply_remote_ai_tab(
        &mut self,
        project_id: &str,
        tab_type: TabType,
        tab_id: &str,
        session_id: &str,
        label: Option<String>,
        session_view: Option<crate::terminal::session::TerminalSessionView>,
    ) {
        self.state.open_ai_tab(
            project_id,
            tab_type.clone(),
            tab_id.to_string(),
            session_id.to_string(),
            label.clone(),
        );
        if let Some(remote_mode) = self.remote_mode.as_mut() {
            remote_mode.snapshot.app_state.open_ai_tab(
                project_id,
                tab_type,
                tab_id.to_string(),
                session_id.to_string(),
                label,
            );
            remote_mode.snapshot.runtime_state.active_session_id = Some(session_id.to_string());
            if let Some(session_view) = session_view {
                remote_mode
                    .snapshot
                    .runtime_state
                    .sessions
                    .insert(session_id.to_string(), session_view.runtime.clone());
                remote_mode
                    .snapshot
                    .session_views
                    .insert(session_id.to_string(), session_view);
            }
            remote_mode
                .client
                .set_focused_session(Some(session_id.to_string()));
        }
        self.show_terminal_surface();
        self.synced_session_id = Some(session_id.to_string());
        self.last_dimensions = None;
        self.terminal_notice = None;
    }

    fn sync_settings_remote_draft(&mut self) {
        if !matches!(self.editor_panel, Some(EditorPanel::Settings(_))) {
            return;
        }

        let connected_label = self
            .remote_mode
            .as_ref()
            .map(|remote_mode| remote_mode.connected_label.clone());
        let connected_endpoint = self
            .remote_mode
            .as_ref()
            .map(|remote_mode| format!("{}:{}", remote_mode.address, remote_mode.port));
        let connected_server_id = self
            .remote_mode
            .as_ref()
            .map(|remote_mode| remote_mode.client.server_id().to_string());
        let connected_fingerprint = self
            .remote_mode
            .as_ref()
            .map(|remote_mode| remote_mode.client.certificate_fingerprint().to_string());
        let remote_reconnect = self
            .remote_mode
            .as_ref()
            .and_then(|remote_mode| remote_mode.reconnect.clone());
        let remote_connected = self.remote_mode.is_some();
        let remote_has_control = self.remote_has_control();
        let remote_status = self.remote_host_service.status();
        let remote_latency_summary = self.remote_mode.as_ref().and_then(|remote_mode| {
            format_remote_latency_summary(&remote_mode.client.latency_stats())
        });
        let remote_port_forwards = self.remote_port_forward_rows();
        let remote_host_latency_summary = format_remote_latency_summary(&remote_status.latency);

        if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
            draft.remote_connected_label = connected_label;
            draft.remote_connected_endpoint = connected_endpoint;
            draft.remote_connected_server_id = connected_server_id;
            draft.remote_connected_fingerprint = connected_fingerprint;
            draft.remote_latency_summary = remote_latency_summary;
            draft.remote_reconnect_attempts = remote_reconnect
                .as_ref()
                .map(|reconnect| reconnect.attempts)
                .unwrap_or(0);
            draft.remote_reconnect_last_error = remote_reconnect
                .as_ref()
                .and_then(|reconnect| reconnect.last_error.clone());
            draft.remote_has_control = remote_has_control;
            draft.remote_connected = remote_connected;
            draft.remote_host_clients = remote_status.connected_clients;
            draft.remote_host_controller_client_id = remote_status.controller_client_id;
            draft.remote_host_listening = remote_status.listening;
            draft.remote_host_error = remote_status.listener_error;
            draft.remote_host_last_note = remote_status.last_connection_note;
            draft.remote_host_last_note_is_error = remote_status.last_connection_is_error;
            draft.remote_host_latency_summary = remote_host_latency_summary;
            draft.remote_host_server_id = self.remote_machine_state.host.server_id.clone();
            draft.remote_host_fingerprint = self
                .remote_machine_state
                .host
                .certificate_fingerprint
                .clone();
            draft.remote_port_forwards = remote_port_forwards;
            draft.remote_known_hosts = self.remote_machine_state.known_hosts.clone();
            draft.remote_paired_clients = self.remote_machine_state.host.paired_clients.clone();
            draft.remote_host_enabled = self.remote_machine_state.host.enabled;
            draft.remote_keep_hosting_in_background =
                self.remote_machine_state.host.keep_hosting_in_background;

            if !draft.remote_connected && draft.remote_connect_address.trim().is_empty() {
                if let Some(host) = draft.remote_known_hosts.first() {
                    draft.remote_connect_address = host.address.clone();
                    draft.remote_connect_port = host.port.to_string();
                }
            }
            if draft.remote_connected && !draft.remote_connect_in_flight {
                if let Some(reconnect) = remote_reconnect.as_ref() {
                    draft.remote_connect_status =
                        draft.remote_connected_label.as_ref().map(|label| {
                            let mut status = format!("Reconnecting to {label}...");
                            if let Some(error) = reconnect.last_error.as_ref() {
                                status.push_str(&format!(" Last error: {error}"));
                            } else if let Some(message) = reconnect.last_disconnect_message.as_ref()
                            {
                                status.push_str(&format!(" {message}"));
                            }
                            status
                        });
                } else {
                    draft.remote_connect_status =
                        draft.remote_connected_label.as_ref().map(|label| {
                            if draft.remote_has_control {
                                format!("Connected to {label}. This client controls the host.")
                            } else {
                                format!("Connected to {label}. This client is in viewer mode.")
                            }
                        });
                }
                draft.remote_connect_status_is_error = false;
            } else if !draft.remote_connected && !draft.remote_connect_in_flight {
                draft.remote_connect_status = self
                    .remote_status_notice
                    .as_ref()
                    .map(|notice| notice.message.clone());
                draft.remote_connect_status_is_error = self
                    .remote_status_notice
                    .as_ref()
                    .map(|notice| notice.is_error)
                    .unwrap_or(false);
            }
        }
    }

    fn sync_remote_host_snapshot_if_due(&mut self, runtime_state: &RuntimeState) {
        if self.remote_mode.is_some() {
            return;
        }

        let remote_status = self.remote_host_service.status();
        if !remote_status.enabled {
            self.last_remote_snapshot_sync_at = None;
            return;
        }

        let has_pending_requests = self.remote_host_service.has_pending_requests();
        let refresh_interval = if remote_status.connected_clients > 0 || has_pending_requests {
            REMOTE_HOST_SNAPSHOT_ACTIVE_INTERVAL
        } else {
            REMOTE_HOST_SNAPSHOT_IDLE_INTERVAL
        };
        let now = Instant::now();
        let is_due = self
            .last_remote_snapshot_sync_at
            .map(|last_sync| now.duration_since(last_sync) >= refresh_interval)
            .unwrap_or(true);
        if !is_due {
            return;
        }

        let runtime_revision = remote_runtime_revision(runtime_state);
        let port_hash = local_stable_hash(&self.server_port_snapshot.statuses);
        let forced_sync = self.last_remote_snapshot_sync_at.is_none();
        if !forced_sync
            && runtime_revision == self.last_remote_runtime_revision
            && port_hash == self.last_remote_port_hash
            && !has_pending_requests
        {
            self.last_remote_snapshot_sync_at = Some(now);
            return;
        }

        self.remote_host_service.update_snapshot(
            self.state.clone(),
            runtime_state.clone(),
            self.server_port_snapshot.statuses.clone(),
        );
        self.last_remote_runtime_revision = runtime_revision;
        self.last_remote_port_hash = port_hash;
        self.last_remote_snapshot_sync_at = Some(now);
    }

    fn sync_remote_host_live_sessions(&mut self, runtime_state: &RuntimeState) {
        if self.remote_mode.is_some() {
            return;
        }

        let remote_status = self.remote_host_service.status();
        if !remote_status.enabled || remote_status.connected_clients == 0 {
            self.remote_live_session_generations.clear();
            let _ = self.process_manager.drain_remote_dirty_sessions();
            return;
        }

        let dirty_sessions = self
            .process_manager
            .drain_remote_dirty_sessions()
            .into_iter()
            .collect::<BTreeSet<_>>();
        let subscribed_session_ids = self.remote_host_service.subscribed_session_ids();

        self.remote_live_session_generations
            .retain(|session_id, _| subscribed_session_ids.contains(session_id));

        for session_id in subscribed_session_ids {
            let generation = runtime_state
                .sessions
                .get(&session_id)
                .map(|session| session.dirty_generation);
            let should_publish = dirty_sessions.contains(&session_id)
                || generation
                    != self
                        .remote_live_session_generations
                        .get(&session_id)
                        .copied();

            if !should_publish {
                continue;
            }

            if let Some(generation) = generation {
                self.remote_live_session_generations
                    .insert(session_id.clone(), generation);
            } else {
                self.remote_live_session_generations.remove(&session_id);
            }
        }
    }

    fn set_remote_status_notice(&mut self, message: impl Into<String>, is_error: bool) {
        self.remote_status_notice = Some(RemoteStatusNotice {
            message: message.into(),
            is_error,
        });
    }

    fn clear_remote_status_notice(&mut self) {
        self.remote_status_notice = None;
    }

    fn preferred_known_remote_host(&self) -> Option<remote::KnownRemoteHost> {
        self.remote_machine_state
            .known_hosts
            .iter()
            .max_by_key(|host| host.last_connected_epoch_ms.unwrap_or(0))
            .cloned()
    }

    fn ensure_remote_settings_open(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.editor_panel,
            Some(EditorPanel::Settings(SettingsDraft {
                remote_focus_only: true,
                ..
            }))
        ) {
            self.sync_settings_remote_draft();
            cx.notify();
            return;
        }
        self.open_settings_panel(true, cx);
    }

    fn copy_remote_pairing_token_action(&mut self, cx: &mut Context<Self>) {
        let token = self.remote_host_service.status().pairing_token;
        if token.trim().is_empty() {
            self.editor_notice =
                Some("Generate or enable hosting before copying a pair token.".to_string());
            self.set_remote_status_notice(
                "Generate or enable hosting before copying a pair token.",
                true,
            );
        } else {
            cx.write_to_clipboard(ClipboardItem::new_string(token));
            self.editor_notice = Some("Copied pair token to the clipboard.".to_string());
            self.set_remote_status_notice("Copied pair token to the clipboard.", false);
        }
        self.sync_settings_remote_draft();
        cx.notify();
    }

    fn connect_known_remote_host(&mut self, host: remote::KnownRemoteHost, cx: &mut Context<Self>) {
        self.editor_notice = None;
        match self.begin_connect_remote_host(host.address.clone(), host.port, None, cx) {
            Ok(()) => {
                self.set_remote_status_notice(format!("Connecting to {}...", host.label), false);
            }
            Err(error) => {
                self.editor_notice = Some(error.clone());
                self.set_remote_status_notice(error.clone(), true);
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.remote_connect_in_flight = false;
                    draft.remote_connect_status = Some(error);
                    draft.remote_connect_status_is_error = true;
                }
            }
        }
    }

    fn connect_preferred_remote_host_action(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.preferred_known_remote_host() else {
            self.ensure_remote_settings_open(cx);
            self.set_remote_status_notice(
                "Save or pair a remote host before using quick connect.",
                true,
            );
            self.sync_settings_remote_draft();
            cx.notify();
            return;
        };
        self.connect_known_remote_host(host, cx);
        cx.notify();
    }

    fn force_remote_reconnect_now(&mut self, cx: &mut Context<Self>) {
        let Some(remote_mode) = self.remote_mode.as_mut() else {
            return;
        };
        let Some(reconnect) = remote_mode.reconnect.as_mut() else {
            return;
        };
        reconnect.next_attempt_at = Instant::now();
        reconnect.in_flight = false;
        reconnect.last_error = None;
        self.try_begin_remote_reconnect(cx);
        self.sync_settings_remote_draft();
        cx.notify();
    }

    #[allow(dead_code)]
    fn remote_status_bar_model(&self) -> chrome::RemoteStatusBarModel {
        let host_status = self.remote_host_service.status();
        let preferred_host = self.preferred_known_remote_host();

        if let Some(remote_mode) = self.remote_mode.as_ref() {
            if remote_mode.reconnect.is_some() {
                return chrome::RemoteStatusBarModel {
                    label: format!("Reconnecting • {}", remote_mode.connected_label),
                    tone: chrome::StatusBarTone::Warning,
                    primary_action: Some(chrome::StatusBarQuickAction {
                        label: "Retry now".to_string(),
                        tone: chrome::StatusBarTone::Accent,
                    }),
                    secondary_action: Some(chrome::StatusBarQuickAction {
                        label: "Disconnect".to_string(),
                        tone: chrome::StatusBarTone::Danger,
                    }),
                    tertiary_action: None,
                };
            }

            return chrome::RemoteStatusBarModel {
                label: format!(
                    "Connected • {} • {}",
                    remote_role_label(remote_mode.snapshot.you_have_control),
                    remote_mode.connected_label
                ),
                tone: if remote_mode.snapshot.you_have_control {
                    chrome::StatusBarTone::Accent
                } else {
                    chrome::StatusBarTone::Warning
                },
                primary_action: Some(chrome::StatusBarQuickAction {
                    label: if remote_mode.snapshot.you_have_control {
                        "Release".to_string()
                    } else {
                        "Take control".to_string()
                    },
                    tone: if remote_mode.snapshot.you_have_control {
                        chrome::StatusBarTone::Muted
                    } else {
                        chrome::StatusBarTone::Accent
                    },
                }),
                secondary_action: Some(chrome::StatusBarQuickAction {
                    label: "Disconnect".to_string(),
                    tone: chrome::StatusBarTone::Danger,
                }),
                tertiary_action: None,
            };
        }

        if host_status.enabled {
            let (label, tone) = if !host_status.listening {
                (
                    format!(
                        "Hosting issue • {}:{}",
                        host_status.bind_address, host_status.port
                    ),
                    chrome::StatusBarTone::Danger,
                )
            } else if self.local_host_has_control() {
                (
                    format!("Hosting • Controller • {}", host_status.port),
                    chrome::StatusBarTone::Success,
                )
            } else {
                (
                    format!("Hosting • Viewer • {}", host_status.port),
                    chrome::StatusBarTone::Warning,
                )
            };
            return chrome::RemoteStatusBarModel {
                label,
                tone,
                primary_action: (!self.local_host_has_control()).then_some(
                    chrome::StatusBarQuickAction {
                        label: "Take local control".to_string(),
                        tone: chrome::StatusBarTone::Accent,
                    },
                ),
                secondary_action: Some(chrome::StatusBarQuickAction {
                    label: "Copy token".to_string(),
                    tone: chrome::StatusBarTone::Accent,
                }),
                tertiary_action: None,
            };
        }

        if self
            .remote_status_notice
            .as_ref()
            .is_some_and(|notice| notice.is_error)
        {
            return chrome::RemoteStatusBarModel {
                label: "Remote error".to_string(),
                tone: chrome::StatusBarTone::Danger,
                primary_action: Some(chrome::StatusBarQuickAction {
                    label: if preferred_host.is_some() {
                        "Connect".to_string()
                    } else {
                        "Remote".to_string()
                    },
                    tone: chrome::StatusBarTone::Accent,
                }),
                secondary_action: None,
                tertiary_action: None,
            };
        }

        chrome::RemoteStatusBarModel {
            label: preferred_host
                .as_ref()
                .map(|host| format!("Local • {}", host.label))
                .unwrap_or_else(|| "Local".to_string()),
            tone: chrome::StatusBarTone::Muted,
            primary_action: Some(chrome::StatusBarQuickAction {
                label: if preferred_host.is_some() {
                    "Connect".to_string()
                } else {
                    "Remote".to_string()
                },
                tone: chrome::StatusBarTone::Accent,
            }),
            secondary_action: None,
            tertiary_action: None,
        }
    }

    fn remote_status_bar_state(&self) -> RemoteStatusBarState {
        let host_status = self.remote_host_service.status();
        let preferred_host = self.preferred_known_remote_host();

        if let Some(remote_mode) = self.remote_mode.as_ref() {
            if remote_mode.reconnect.is_some() {
                return RemoteStatusBarState {
                    model: chrome::RemoteStatusBarModel {
                        label: format!("Reconnecting • {}", remote_mode.connected_label),
                        tone: chrome::StatusBarTone::Warning,
                        primary_action: Some(chrome::StatusBarQuickAction {
                            label: "Retry now".to_string(),
                            tone: chrome::StatusBarTone::Accent,
                        }),
                        secondary_action: Some(chrome::StatusBarQuickAction {
                            label: "Disconnect".to_string(),
                            tone: chrome::StatusBarTone::Danger,
                        }),
                        tertiary_action: None,
                    },
                    primary_action: Some(RemoteStatusBarAction::RetryReconnect),
                    secondary_action: Some(RemoteStatusBarAction::DisconnectRemote),
                    tertiary_action: None,
                };
            }

            return RemoteStatusBarState {
                model: chrome::RemoteStatusBarModel {
                    label: format!(
                        "Connected • {} • {}",
                        remote_role_label(remote_mode.snapshot.you_have_control),
                        remote_mode.connected_label
                    ),
                    tone: if remote_mode.snapshot.you_have_control {
                        chrome::StatusBarTone::Accent
                    } else {
                        chrome::StatusBarTone::Warning
                    },
                    primary_action: Some(chrome::StatusBarQuickAction {
                        label: if remote_mode.snapshot.you_have_control {
                            "Release".to_string()
                        } else {
                            "Take control".to_string()
                        },
                        tone: if remote_mode.snapshot.you_have_control {
                            chrome::StatusBarTone::Muted
                        } else {
                            chrome::StatusBarTone::Accent
                        },
                    }),
                    secondary_action: Some(chrome::StatusBarQuickAction {
                        label: "Disconnect".to_string(),
                        tone: chrome::StatusBarTone::Danger,
                    }),
                    tertiary_action: None,
                },
                primary_action: Some(if remote_mode.snapshot.you_have_control {
                    RemoteStatusBarAction::ReleaseRemoteControl
                } else {
                    RemoteStatusBarAction::TakeRemoteControl
                }),
                secondary_action: Some(RemoteStatusBarAction::DisconnectRemote),
                tertiary_action: None,
            };
        }

        if host_status.enabled {
            let (label, tone) = if !host_status.listening {
                (
                    format!(
                        "Hosting issue • {}:{}",
                        host_status.bind_address, host_status.port
                    ),
                    chrome::StatusBarTone::Danger,
                )
            } else if self.local_host_has_control() {
                (
                    format!("Hosting • Controller • {}", host_status.port),
                    chrome::StatusBarTone::Success,
                )
            } else {
                (
                    format!("Hosting • Viewer • {}", host_status.port),
                    chrome::StatusBarTone::Warning,
                )
            };
            return RemoteStatusBarState {
                model: chrome::RemoteStatusBarModel {
                    label,
                    tone,
                    primary_action: (!self.local_host_has_control()).then_some(
                        chrome::StatusBarQuickAction {
                            label: "Take local control".to_string(),
                            tone: chrome::StatusBarTone::Accent,
                        },
                    ),
                    secondary_action: Some(chrome::StatusBarQuickAction {
                        label: "Copy token".to_string(),
                        tone: chrome::StatusBarTone::Accent,
                    }),
                    tertiary_action: None,
                },
                primary_action: (!self.local_host_has_control())
                    .then_some(RemoteStatusBarAction::TakeHostControl),
                secondary_action: Some(RemoteStatusBarAction::CopyPairToken),
                tertiary_action: None,
            };
        }

        if self
            .remote_status_notice
            .as_ref()
            .is_some_and(|notice| notice.is_error)
        {
            return RemoteStatusBarState {
                model: chrome::RemoteStatusBarModel {
                    label: "Remote error".to_string(),
                    tone: chrome::StatusBarTone::Danger,
                    primary_action: Some(chrome::StatusBarQuickAction {
                        label: if preferred_host.is_some() {
                            "Connect".to_string()
                        } else {
                            "Remote".to_string()
                        },
                        tone: chrome::StatusBarTone::Accent,
                    }),
                    secondary_action: None,
                    tertiary_action: None,
                },
                primary_action: Some(if preferred_host.is_some() {
                    RemoteStatusBarAction::ConnectPreferred
                } else {
                    RemoteStatusBarAction::OpenRemoteSettings
                }),
                secondary_action: None,
                tertiary_action: None,
            };
        }

        RemoteStatusBarState {
            model: chrome::RemoteStatusBarModel {
                label: preferred_host
                    .as_ref()
                    .map(|host| format!("Local • {}", host.label))
                    .unwrap_or_else(|| "Local".to_string()),
                tone: chrome::StatusBarTone::Muted,
                primary_action: Some(chrome::StatusBarQuickAction {
                    label: if preferred_host.is_some() {
                        "Connect".to_string()
                    } else {
                        "Remote".to_string()
                    },
                    tone: chrome::StatusBarTone::Accent,
                }),
                secondary_action: None,
                tertiary_action: None,
            },
            primary_action: Some(if preferred_host.is_some() {
                RemoteStatusBarAction::ConnectPreferred
            } else {
                RemoteStatusBarAction::OpenRemoteSettings
            }),
            secondary_action: None,
            tertiary_action: None,
        }
    }

    fn prepare_remote_connect(
        &self,
        address: String,
        port: u16,
        pairing_token: Option<String>,
    ) -> Result<PreparedRemoteConnect, String> {
        let known_host = self
            .remote_machine_state
            .known_hosts
            .iter()
            .find(|host| host.address == address && host.port == port)
            .cloned();
        let auth = if let Some(token) = pairing_token.filter(|token| !token.trim().is_empty()) {
            ClientAuth::PairToken {
                token: token.trim().to_string(),
            }
        } else if let Some(host) = known_host.clone() {
            ClientAuth::ClientToken {
                client_id: host.client_id,
                auth_token: host.auth_token,
            }
        } else {
            return Err("Pair with a host token the first time you connect.".to_string());
        };

        let host_label = format!("{address}:{port}");
        let expected_fingerprint = known_host
            .as_ref()
            .map(|host| host.certificate_fingerprint.trim())
            .filter(|fingerprint| !fingerprint.is_empty());
        Ok(PreparedRemoteConnect {
            address,
            port,
            host_label,
            auth,
            expected_fingerprint: expected_fingerprint.map(str::to_string),
            known_server_id: known_host.as_ref().map(|host| host.server_id.clone()),
        })
    }

    fn apply_connected_remote_host(
        &mut self,
        address: String,
        port: u16,
        host_label: String,
        client: RemoteClientHandle,
        pool_key: String,
        snapshot: remote::RemoteWorkspaceSnapshot,
        server_id: String,
        certificate_fingerprint: String,
        client_id: String,
        client_token: String,
    ) {
        client.take_control();
        let snapshot = client.latest_snapshot().unwrap_or(snapshot);
        let address_for_mode = address.clone();
        let replacing_remote = self.remote_mode.is_some();
        if self.local_state_backup.is_none() {
            self.local_state_backup = Some(self.state.clone());
        }
        if let Some(existing) = self.remote_mode.take() {
            existing.port_forwards.shutdown();
            self.remote_client_pool.remove(&existing.pool_key);
            existing.client.disconnect();
        }
        remote::upsert_known_host(
            &mut self.remote_machine_state,
            host_label.clone(),
            address,
            port,
            server_id.clone(),
            certificate_fingerprint,
            client_id,
            client_token,
        );
        self.persist_remote_machine_state();
        let port_forwards = LocalPortForwardManager::new(client.clone());
        self.remote_mode = Some(RemoteModeState {
            subscribed_session_ids: BTreeSet::new(),
            last_snapshot_revision: client.snapshot_revision(),
            last_session_stream_revision: client.session_stream_revision(),
            client,
            port_forwards,
            snapshot: snapshot.clone(),
            connected_label: host_label,
            address: address_for_mode,
            port,
            pool_key,
            reconnect: None,
        });
        self.state = self.merge_remote_snapshot_into_state(&snapshot);
        let _ = self.sync_remote_port_forwards();
        self.sync_remote_session_subscriptions();
        self.editor_notice =
            (!replacing_remote).then_some("Connected to remote host and took control.".to_string());
        self.terminal_notice = None;
        self.clear_remote_status_notice();
        self.sync_settings_remote_draft();
    }

    fn begin_remote_reconnect(&mut self, message: String, cx: &mut Context<Self>) {
        let Some(remote_mode) = self.remote_mode.as_mut() else {
            return;
        };
        let mut pool_key_to_remove = None;
        let mut client_to_disconnect = None;
        if remote_mode.reconnect.is_none() {
            remote_mode.port_forwards.shutdown();
            pool_key_to_remove = Some(remote_mode.pool_key.clone());
            client_to_disconnect = Some(remote_mode.client.clone());
            remote_mode.reconnect = Some(RemoteReconnectState {
                attempts: 0,
                next_attempt_at: Instant::now(),
                in_flight: false,
                last_disconnect_message: Some(message),
                last_error: None,
            });
        } else if let Some(reconnect) = remote_mode.reconnect.as_mut() {
            reconnect.last_disconnect_message = Some(message);
        }
        if let Some(pool_key) = pool_key_to_remove {
            self.remote_client_pool.remove(&pool_key);
        }
        if let Some(client) = client_to_disconnect {
            client.disconnect();
        }
        self.terminal_notice = Some("Reconnecting to remote host...".to_string());
        self.set_remote_status_notice("Reconnecting to remote host...", false);
        self.sync_settings_remote_draft();
        self.try_begin_remote_reconnect(cx);
    }

    fn try_begin_remote_reconnect(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((address, port)) = self.remote_mode.as_ref().and_then(|remote_mode| {
            let reconnect = remote_mode.reconnect.as_ref()?;
            if reconnect.in_flight || Instant::now() < reconnect.next_attempt_at {
                return None;
            }
            Some((remote_mode.address.clone(), remote_mode.port))
        }) else {
            return false;
        };

        let prepared = match self.prepare_remote_connect(address, port, None) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.disconnect_remote_host(Some(error));
                return true;
            }
        };

        if let Some(remote_mode) = self.remote_mode.as_mut() {
            if let Some(reconnect) = remote_mode.reconnect.as_mut() {
                reconnect.in_flight = true;
                reconnect.last_error = None;
            }
        }
        self.remote_connect_request_id = self.remote_connect_request_id.saturating_add(1);
        let request_id = self.remote_connect_request_id;
        let background_executor = cx.background_executor().clone();
        let pool = self.remote_client_pool.clone();
        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let mut async_cx = cx.clone();
                let prepared = prepared;
                async move {
                    let address = prepared.address.clone();
                    let port = prepared.port;
                    let host_label = prepared.host_label.clone();
                    let expected_fingerprint = prepared.expected_fingerprint.clone();
                    let result = background_executor
                        .spawn(async move {
                            RemoteClientHandle::connect(
                                &prepared.address,
                                prepared.port,
                                "DevManager",
                                prepared.auth,
                                expected_fingerprint.as_deref(),
                            )
                        })
                        .await;
                    let _ = this.update(&mut async_cx, |this, cx: &mut Context<'_, Self>| {
                        if request_id != this.remote_connect_request_id {
                            return;
                        }
                        match result {
                            Ok(result) => {
                                let pool_key = pool.insert(
                                    address.clone(),
                                    port,
                                    result.server_id.clone(),
                                    result.certificate_fingerprint.clone(),
                                    result.client.clone(),
                                );
                                this.apply_connected_remote_host(
                                    address,
                                    port,
                                    host_label,
                                    result.client,
                                    pool_key,
                                    result.snapshot,
                                    result.server_id,
                                    result.certificate_fingerprint,
                                    result.client_id,
                                    result.client_token,
                                );
                                this.terminal_notice = None;
                            }
                            Err(error) => {
                                if fatal_remote_reconnect_error(&error) {
                                    this.disconnect_remote_host(Some(error));
                                    cx.notify();
                                    return;
                                }
                                if let Some(remote_mode) = this.remote_mode.as_mut() {
                                    if let Some(reconnect) = remote_mode.reconnect.as_mut() {
                                        reconnect.in_flight = false;
                                        reconnect.attempts = reconnect.attempts.saturating_add(1);
                                        reconnect.last_error = Some(error);
                                        reconnect.next_attempt_at = Instant::now()
                                            + remote_reconnect_backoff(reconnect.attempts);
                                    }
                                }
                                this.sync_settings_remote_draft();
                            }
                        }
                        cx.notify();
                    });
                }
            },
        )
        .detach();
        true
    }

    fn begin_connect_remote_host(
        &mut self,
        address: String,
        port: u16,
        pairing_token: Option<String>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let prepared = self.prepare_remote_connect(address, port, pairing_token)?;
        if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
            if draft.remote_connect_in_flight {
                return Ok(());
            }
            draft.remote_connect_in_flight = true;
            draft.remote_connect_status = Some(format!("Connecting to {}...", prepared.host_label));
            draft.remote_connect_status_is_error = false;
        }
        self.set_remote_status_notice(format!("Connecting to {}...", prepared.host_label), false);

        if let Some((pool_key, client)) = self.remote_client_pool.get_reusable(
            &prepared.address,
            prepared.port,
            prepared.known_server_id.as_deref(),
            prepared.expected_fingerprint.as_deref(),
        ) {
            self.apply_connected_remote_host(
                prepared.address,
                prepared.port,
                prepared.host_label.clone(),
                client.clone(),
                pool_key,
                client.latest_snapshot().unwrap_or_default(),
                client.server_id().to_string(),
                client.certificate_fingerprint().to_string(),
                client.client_id().to_string(),
                client.client_token().to_string(),
            );
            if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                draft.remote_connect_in_flight = false;
                draft.remote_connect_token.clear();
                draft.remote_connect_status = Some(format!(
                    "Connected to {} and took control.",
                    prepared.host_label
                ));
                draft.remote_connect_status_is_error = false;
            }
            self.clear_remote_status_notice();
            self.sync_settings_remote_draft();
            cx.notify();
            return Ok(());
        }

        self.remote_connect_request_id = self.remote_connect_request_id.saturating_add(1);
        let request_id = self.remote_connect_request_id;
        let background_executor = cx.background_executor().clone();
        let pool = self.remote_client_pool.clone();
        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let mut async_cx = cx.clone();
                let prepared = prepared;
                async move {
                    let address = prepared.address.clone();
                    let port = prepared.port;
                    let host_label = prepared.host_label.clone();
                    let expected_fingerprint = prepared.expected_fingerprint.clone();
                    let result = background_executor
                        .spawn(async move {
                            RemoteClientHandle::connect(
                                &prepared.address,
                                prepared.port,
                                "DevManager",
                                prepared.auth,
                                expected_fingerprint.as_deref(),
                            )
                        })
                        .await;
                    let _ = this.update(&mut async_cx, |this, cx: &mut Context<'_, Self>| {
                        if request_id != this.remote_connect_request_id {
                            return;
                        }
                        if let Some(EditorPanel::Settings(draft)) = this.editor_panel.as_mut() {
                            draft.remote_connect_in_flight = false;
                        }
                        match result {
                            Ok(result) => {
                                let pool_key = pool.insert(
                                    address.clone(),
                                    port,
                                    result.server_id.clone(),
                                    result.certificate_fingerprint.clone(),
                                    result.client.clone(),
                                );
                                this.apply_connected_remote_host(
                                    address,
                                    port,
                                    host_label.clone(),
                                    result.client,
                                    pool_key,
                                    result.snapshot,
                                    result.server_id,
                                    result.certificate_fingerprint,
                                    result.client_id,
                                    result.client_token,
                                );
                                if let Some(EditorPanel::Settings(draft)) =
                                    this.editor_panel.as_mut()
                                {
                                    draft.remote_connect_token.clear();
                                    draft.remote_connect_status = Some(format!(
                                        "Connected to {} and took control.",
                                        host_label
                                    ));
                                    draft.remote_connect_status_is_error = false;
                                }
                                this.clear_remote_status_notice();
                            }
                            Err(error) => {
                                this.editor_notice = Some(error.clone());
                                this.set_remote_status_notice(error.clone(), true);
                                if let Some(EditorPanel::Settings(draft)) =
                                    this.editor_panel.as_mut()
                                {
                                    draft.remote_connect_status = Some(error);
                                    draft.remote_connect_status_is_error = true;
                                }
                            }
                        }
                        this.sync_settings_remote_draft();
                        cx.notify();
                    });
                }
            },
        )
        .detach();
        cx.notify();
        Ok(())
    }

    fn disconnect_remote_host(&mut self, message: Option<String>) {
        self.remote_connect_request_id = self.remote_connect_request_id.saturating_add(1);
        if let Some(remote_mode) = self.remote_mode.take() {
            remote_mode.port_forwards.shutdown();
            self.remote_client_pool.remove(&remote_mode.pool_key);
            remote_mode.client.disconnect();
        }
        if let Some(local_state) = self.local_state_backup.take() {
            self.state = local_state;
        }
        self.synced_session_id = None;
        self.last_dimensions = None;
        self.remote_live_session_generations.clear();
        self.terminal_notice = message;
        let remote_status_is_error = self
            .terminal_notice
            .as_deref()
            .map(fatal_remote_reconnect_error)
            .unwrap_or(false);
        if let Some(message) = self.terminal_notice.clone() {
            self.set_remote_status_notice(message, remote_status_is_error);
        }
        if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
            draft.remote_connect_in_flight = false;
            draft.remote_connect_status = self.terminal_notice.clone();
            draft.remote_connect_status_is_error = remote_status_is_error;
        }
        self.sync_settings_remote_draft();
    }

    fn current_runtime_snapshot(&self) -> RuntimeState {
        self.remote_mode
            .as_ref()
            .map(|remote_mode| remote_mode.snapshot.runtime_state.clone())
            .unwrap_or_else(|| self.process_manager.runtime_state())
    }

    fn local_viewer_session_view(
        &mut self,
        session_id: &str,
        dimensions: SessionDimensions,
    ) -> Option<crate::terminal::session::TerminalSessionView> {
        let live_view = self.process_manager.session_view(session_id)?;
        let dirty_generation = live_view.runtime.dirty_generation;
        let should_rebuild = self
            .local_viewer_replicas
            .get(session_id)
            .map(|state| state.dirty_generation != dirty_generation)
            .unwrap_or(true);

        if should_rebuild {
            let replay_bytes = self.process_manager.session_replay_bytes(session_id).ok()?;
            self.local_viewer_replicas.insert(
                session_id.to_string(),
                LocalViewerReplicaState {
                    dirty_generation,
                    replica: crate::terminal::session::TerminalReplica::from_bootstrap(
                        session_id.to_string(),
                        live_view.runtime.clone(),
                        &replay_bytes,
                    ),
                },
            );
        }

        let state = self.local_viewer_replicas.get_mut(session_id)?;
        state.replica.apply_local_resize(dimensions);
        state.replica.view()
    }

    fn current_port_statuses(&self) -> HashMap<u16, PortStatus> {
        self.remote_mode
            .as_ref()
            .map(|remote_mode| remote_mode.snapshot.port_statuses.clone())
            .unwrap_or_else(|| self.server_port_snapshot.statuses.clone())
    }

    fn sync_remote_port_forwards(&mut self) -> bool {
        let Some(remote_mode) = self.remote_mode.as_ref() else {
            return false;
        };
        remote_mode
            .port_forwards
            .sync_ports(&remote_forwardable_ports(&remote_mode.snapshot))
    }

    fn remote_port_forward_state(&self, port: u16) -> Option<RemotePortForwardState> {
        self.remote_mode
            .as_ref()
            .and_then(|remote_mode| remote_mode.port_forwards.state_for(port))
    }

    fn remote_port_forward_rows(&self) -> Vec<RemotePortForwardDraft> {
        let Some(remote_mode) = self.remote_mode.as_ref() else {
            return Vec::new();
        };
        let statuses = remote_mode.port_forwards.statuses();
        remote_port_forward_rows(&remote_mode.snapshot, &statuses)
    }

    fn active_remote_terminal_session_id(&self) -> Option<String> {
        let tab = self.state.active_tab()?;
        match tab.tab_type {
            TabType::Server => tab.command_id.clone(),
            TabType::Claude | TabType::Codex | TabType::Ssh => tab
                .pty_session_id
                .clone()
                .or_else(|| tab.command_id.clone()),
        }
    }

    fn resolved_terminal_session_id(
        &self,
        active_session: Option<&crate::terminal::session::TerminalSessionView>,
    ) -> Option<String> {
        active_session
            .map(|session| session.runtime.session_id.clone())
            .or_else(|| {
                if self.remote_mode.is_some() {
                    self.active_remote_terminal_session_id()
                } else {
                    Some(self.state.active_terminal_spec().session_id)
                }
            })
    }

    fn current_active_session_view(&self) -> Option<crate::terminal::session::TerminalSessionView> {
        if let Some(remote_mode) = self.remote_mode.as_ref() {
            let active_session_id = self.active_remote_terminal_session_id()?;
            return remote_mode
                .client
                .session_view(&active_session_id)
                .or_else(|| {
                    remote_mode
                        .snapshot
                        .session_views
                        .get(&active_session_id)
                        .cloned()
                });
        }
        self.process_manager.active_session()
    }

    fn current_session_view(
        &self,
        session_id: &str,
    ) -> Option<crate::terminal::session::TerminalSessionView> {
        if let Some(remote_mode) = self.remote_mode.as_ref() {
            return remote_mode
                .client
                .session_view(session_id)
                .or_else(|| remote_mode.snapshot.session_views.get(session_id).cloned());
        }
        self.process_manager.session_view(session_id)
    }

    fn spawn_remote_git_request_if_needed(
        &mut self,
        action: &RemoteAction,
        response: Option<std::sync::mpsc::Sender<RemoteActionResult>>,
        cx: &mut Context<Self>,
    ) -> bool {
        #[derive(Debug)]
        enum GitHostMutation {
            SetGithubToken(Option<String>),
        }

        let action = match action {
            RemoteAction::GitListRepos
            | RemoteAction::GitStatus { .. }
            | RemoteAction::GitLog { .. }
            | RemoteAction::GitDiffFile { .. }
            | RemoteAction::GitDiffCommit { .. }
            | RemoteAction::GitBranches { .. }
            | RemoteAction::GitStage { .. }
            | RemoteAction::GitUnstage { .. }
            | RemoteAction::GitStageAll { .. }
            | RemoteAction::GitUnstageAll { .. }
            | RemoteAction::GitCommit { .. }
            | RemoteAction::GitPush { .. }
            | RemoteAction::GitPushSetUpstream { .. }
            | RemoteAction::GitPull { .. }
            | RemoteAction::GitFetch { .. }
            | RemoteAction::GitSync { .. }
            | RemoteAction::GitSwitchBranch { .. }
            | RemoteAction::GitCreateBranch { .. }
            | RemoteAction::GitDeleteBranch { .. }
            | RemoteAction::GitGetGithubAuthStatus
            | RemoteAction::GitRequestDeviceCode
            | RemoteAction::GitPollForToken { .. }
            | RemoteAction::GitLogout
            | RemoteAction::GitGenerateCommitMessage { .. } => action.clone(),
            _ => return false,
        };

        let projects = self.state.projects().to_vec();
        let host_token = self
            .state
            .settings()
            .github_token
            .clone()
            .filter(|token| !token.trim().is_empty());

        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let mut cx = cx.clone();
                async move {
                let (result, mutation) = cx
                    .background_executor()
                    .spawn(async move {
                        match action {
                            RemoteAction::GitListRepos => (
                                RemoteActionResult::ok(
                                    None,
                                    Some(RemoteActionPayload::GitRepos {
                                        repos: collect_git_repositories_from_projects(&projects),
                                    }),
                                ),
                                None,
                            ),
                            RemoteAction::GitStatus { repo_path } => (
                                match git_service::status(&repo_path) {
                                    Ok(status) => RemoteActionResult::ok(
                                        None,
                                        Some(RemoteActionPayload::GitStatus { status }),
                                    ),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitLog {
                                repo_path,
                                limit,
                                skip,
                            } => (
                                match git_service::log(&repo_path, limit, skip) {
                                    Ok(entries) => RemoteActionResult::ok(
                                        None,
                                        Some(RemoteActionPayload::GitLogEntries { entries }),
                                    ),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitDiffFile {
                                repo_path,
                                file_path,
                                staged,
                            } => (
                                match git_service::diff_file(&repo_path, &file_path, staged) {
                                    Ok(diff) => RemoteActionResult::ok(
                                        None,
                                        Some(RemoteActionPayload::GitDiff { diff }),
                                    ),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitDiffCommit { repo_path, hash } => (
                                match git_service::diff_commit(&repo_path, &hash) {
                                    Ok(diff) => RemoteActionResult::ok(
                                        None,
                                        Some(RemoteActionPayload::GitDiff { diff }),
                                    ),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitBranches { repo_path } => (
                                match git_service::branches(&repo_path) {
                                    Ok(branches) => RemoteActionResult::ok(
                                        None,
                                        Some(RemoteActionPayload::GitBranches { branches }),
                                    ),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitStage { repo_path, files } => {
                                let file_refs =
                                    files.iter().map(|file| file.as_str()).collect::<Vec<_>>();
                                (
                                    match git_service::stage(&repo_path, &file_refs) {
                                        Ok(()) => RemoteActionResult::ok(None, None),
                                        Err(error) => RemoteActionResult::error(error),
                                    },
                                    None,
                                )
                            }
                            RemoteAction::GitUnstage { repo_path, files } => {
                                let file_refs =
                                    files.iter().map(|file| file.as_str()).collect::<Vec<_>>();
                                (
                                    match git_service::unstage(&repo_path, &file_refs) {
                                        Ok(()) => RemoteActionResult::ok(None, None),
                                        Err(error) => RemoteActionResult::error(error),
                                    },
                                    None,
                                )
                            }
                            RemoteAction::GitStageAll { repo_path } => (
                                match git_service::stage_all(&repo_path) {
                                    Ok(()) => RemoteActionResult::ok(None, None),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitUnstageAll { repo_path } => (
                                match git_service::unstage_all(&repo_path) {
                                    Ok(()) => RemoteActionResult::ok(None, None),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitCommit {
                                repo_path,
                                summary,
                                body,
                            } => (
                                match git_service::commit(&repo_path, &summary, body.as_deref()) {
                                    Ok(hash) => RemoteActionResult::ok(
                                        None,
                                        Some(RemoteActionPayload::GitCommit { hash }),
                                    ),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitPush { repo_path } => (
                                match git_service::push(&repo_path) {
                                    Ok(message) => RemoteActionResult::ok(Some(message), None),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitPushSetUpstream { repo_path, branch } => (
                                match git_service::push_set_upstream(&repo_path, &branch) {
                                    Ok(message) => RemoteActionResult::ok(Some(message), None),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitPull { repo_path } => (
                                match git_service::pull(&repo_path) {
                                    Ok(message) => RemoteActionResult::ok(Some(message), None),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitFetch { repo_path } => (
                                match git_service::fetch(&repo_path) {
                                    Ok(message) => RemoteActionResult::ok(Some(message), None),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitSync { repo_path } => (
                                match git_service::sync(&repo_path) {
                                    Ok(message) => RemoteActionResult::ok(Some(message), None),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitSwitchBranch { repo_path, name } => (
                                match git_service::switch_branch(&repo_path, &name) {
                                    Ok(()) => RemoteActionResult::ok(None, None),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitCreateBranch { repo_path, name } => (
                                match git_service::create_branch(&repo_path, &name) {
                                    Ok(()) => RemoteActionResult::ok(None, None),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitDeleteBranch { repo_path, name } => (
                                match git_service::delete_branch(&repo_path, &name) {
                                    Ok(()) => RemoteActionResult::ok(None, None),
                                    Err(error) => RemoteActionResult::error(error),
                                },
                                None,
                            ),
                            RemoteAction::GitGetGithubAuthStatus => {
                                if let Some(token) = host_token.clone() {
                                    let username = git_service::get_github_username(&token).ok();
                                    (
                                        RemoteActionResult::ok(
                                            None,
                                            Some(RemoteActionPayload::GitAuthStatus {
                                                has_token: true,
                                                username,
                                            }),
                                        ),
                                        None,
                                    )
                                } else {
                                    (
                                        RemoteActionResult::ok(
                                            None,
                                            Some(RemoteActionPayload::GitAuthStatus {
                                                has_token: false,
                                                username: None,
                                            }),
                                        ),
                                        None,
                                    )
                                }
                            }
                            RemoteAction::GitRequestDeviceCode => {
                                if let Some(client_id) = git_service::get_github_client_id() {
                                    (
                                        match git_service::request_device_code(&client_id) {
                                            Ok(device_code) => RemoteActionResult::ok(
                                                None,
                                                Some(RemoteActionPayload::GitDeviceCode {
                                                    device_code,
                                                }),
                                            ),
                                            Err(error) => RemoteActionResult::error(error),
                                        },
                                        None,
                                    )
                                } else {
                                    (
                                        RemoteActionResult::error(
                                            "Set DEVMANAGER_GITHUB_CLIENT_ID on the host, or use the built-in default GitHub OAuth app.",
                                        ),
                                        None,
                                    )
                                }
                            }
                            RemoteAction::GitPollForToken { device_code } => {
                                if let Some(client_id) = git_service::get_github_client_id() {
                                    match git_service::poll_for_token(&client_id, &device_code) {
                                        Ok(Some(token_resp)) => {
                                            let username = git_service::get_github_username(
                                                &token_resp.access_token,
                                            )
                                            .ok();
                                            (
                                                RemoteActionResult::ok(
                                                    None,
                                                    Some(RemoteActionPayload::GitTokenPoll {
                                                        completed: true,
                                                        username,
                                                    }),
                                                ),
                                                Some(GitHostMutation::SetGithubToken(Some(
                                                    token_resp.access_token,
                                                ))),
                                            )
                                        }
                                        Ok(None) => (
                                            RemoteActionResult::ok(
                                                None,
                                                Some(RemoteActionPayload::GitTokenPoll {
                                                    completed: false,
                                                    username: None,
                                                }),
                                            ),
                                            None,
                                        ),
                                        Err(error) => (RemoteActionResult::error(error), None),
                                    }
                                } else {
                                    (
                                        RemoteActionResult::error(
                                            "Set DEVMANAGER_GITHUB_CLIENT_ID on the host, or use the built-in default GitHub OAuth app.",
                                        ),
                                        None,
                                    )
                                }
                            }
                            RemoteAction::GitLogout => (
                                RemoteActionResult::ok(None, None),
                                Some(GitHostMutation::SetGithubToken(None)),
                            ),
                            RemoteAction::GitGenerateCommitMessage { repo_path } => {
                                if let Some(token) = host_token.clone() {
                                    match git_service::get_staged_diff(&repo_path) {
                                        Ok(diff) if diff.trim().is_empty() => (
                                            RemoteActionResult::error(
                                                "No staged changes to summarize",
                                            ),
                                            None,
                                        ),
                                        Ok(diff) => match git_service::generate_commit_message(
                                            &token, &diff,
                                        ) {
                                            Ok(message) => (
                                                RemoteActionResult::ok(
                                                    None,
                                                    Some(
                                                        RemoteActionPayload::GitCommitMessage {
                                                            message,
                                                        },
                                                    ),
                                                ),
                                                None,
                                            ),
                                            Err(error) => (
                                                RemoteActionResult::error(format!("AI: {error}")),
                                                None,
                                            ),
                                        },
                                        Err(error) => (RemoteActionResult::error(error), None),
                                    }
                                } else {
                                    (
                                        RemoteActionResult::error(
                                            "GitHub token not configured on the host.",
                                        ),
                                        None,
                                    )
                                }
                            }
                            _ => unreachable!("non-git action dispatched to git worker"),
                        }
                    })
                    .await;

                if let Some(mutation) = mutation {
                    let _ = this.update(&mut cx, |this, cx| {
                        match mutation {
                            GitHostMutation::SetGithubToken(token) => {
                                let mut settings = this.state.settings().clone();
                                settings.github_token = token;
                                this.state.update_settings(settings);
                                this.save_config_state();
                                this.last_remote_snapshot_sync_at = None;
                                this.sync_settings_remote_draft();
                                cx.notify();
                            }
                        }
                    });
                }

                if let Some(response) = response {
                    let _ = response.send(result);
                }
                }
            },
        )
        .detach();

        true
    }

    fn pump_remote_host_requests(&mut self, cx: &mut Context<Self>) -> bool {
        let requests = self.remote_host_service.drain_requests();
        if requests.is_empty() {
            return false;
        }

        let mut did_change = false;

        for request in requests {
            let PendingRemoteRequest {
                client_id: _client_id,
                action,
                response,
            } = request;

            if self.spawn_remote_git_request_if_needed(&action, response.clone(), cx) {
                continue;
            }

            let result = match action {
                RemoteAction::StartServer {
                    command_id,
                    focus,
                    dimensions,
                } => {
                    let result = if focus {
                        self.process_manager
                            .start_server(&mut self.state, &command_id, dimensions)
                    } else {
                        self.process_manager.start_server_in_background(
                            &mut self.state,
                            &command_id,
                            dimensions,
                        )
                    };
                    match result {
                        Ok(()) => {
                            did_change = true;
                            self.save_session_state();
                            RemoteActionResult::ok(None, None)
                        }
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::StopServer { command_id } => {
                    match self.process_manager.stop_server(&command_id) {
                        Ok(()) => {
                            did_change = true;
                            self.save_session_state();
                            RemoteActionResult::ok(None, None)
                        }
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::RestartServer {
                    command_id,
                    dimensions,
                } => match self.process_manager.restart_server(
                    &mut self.state,
                    &command_id,
                    dimensions,
                ) {
                    Ok(()) => {
                        did_change = true;
                        self.save_session_state();
                        RemoteActionResult::ok(None, None)
                    }
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::LaunchAi {
                    project_id,
                    tab_type,
                    dimensions,
                } => match self.process_manager.start_ai_session(
                    &mut self.state,
                    &project_id,
                    tab_type,
                    dimensions,
                ) {
                    Ok(session_id) => {
                        did_change = true;
                        self.save_session_state();
                        RemoteActionResult::ok(
                            Some(format!("Opened {session_id}")),
                            remote_ai_tab_payload(
                                &self.state,
                                &session_id,
                                self.process_manager.session_view(&session_id),
                            ),
                        )
                    }
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::OpenAiTab { tab_id, dimensions } => match self
                    .process_manager
                    .ensure_ai_session_for_tab(&mut self.state, &tab_id, dimensions, true, false)
                {
                    Ok(session_id) => {
                        did_change = true;
                        self.save_session_state();
                        RemoteActionResult::ok(
                            None,
                            remote_ai_tab_payload(
                                &self.state,
                                &session_id,
                                self.process_manager.session_view(&session_id),
                            ),
                        )
                    }
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::RestartAiTab { tab_id, dimensions } => match self
                    .process_manager
                    .restart_ai_session(&mut self.state, &tab_id, dimensions)
                {
                    Ok(session_id) => {
                        did_change = true;
                        self.save_session_state();
                        RemoteActionResult::ok(
                            None,
                            remote_ai_tab_payload(
                                &self.state,
                                &session_id,
                                self.process_manager.session_view(&session_id),
                            ),
                        )
                    }
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::CloseAiTab { tab_id } => match self
                    .process_manager
                    .close_ai_session(&mut self.state, &tab_id)
                {
                    Ok(()) => {
                        did_change = true;
                        self.save_session_state();
                        RemoteActionResult::ok(None, None)
                    }
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::OpenSshTab { connection_id } => {
                    if let Some(connection) =
                        self.state.find_ssh_connection(&connection_id).cloned()
                    {
                        let project_id = self
                            .state
                            .find_ssh_tab_by_connection(&connection_id)
                            .map(|tab| tab.project_id.clone())
                            .or_else(|| {
                                self.state
                                    .active_project()
                                    .map(|project| project.id.clone())
                            })
                            .or_else(|| {
                                self.state
                                    .projects()
                                    .first()
                                    .map(|project| project.id.clone())
                            })
                            .unwrap_or_default();
                        self.state.open_ssh_tab(
                            &project_id,
                            &connection_id,
                            Some(connection.label),
                        );
                        did_change = true;
                        self.save_session_state();
                        RemoteActionResult::ok(None, None)
                    } else {
                        RemoteActionResult::error(format!(
                            "Unknown SSH connection `{connection_id}`"
                        ))
                    }
                }
                RemoteAction::ConnectSsh {
                    connection_id,
                    dimensions,
                } => match self.process_manager.start_ssh_session(
                    &mut self.state,
                    &connection_id,
                    dimensions,
                ) {
                    Ok(_) => {
                        did_change = true;
                        self.save_session_state();
                        RemoteActionResult::ok(None, None)
                    }
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::RestartSsh {
                    connection_id,
                    dimensions,
                } => {
                    let Some(tab_id) = self
                        .state
                        .find_ssh_tab_by_connection(&connection_id)
                        .map(|tab| tab.id.clone())
                    else {
                        match self.process_manager.start_ssh_session(
                            &mut self.state,
                            &connection_id,
                            dimensions,
                        ) {
                            Ok(_) => {
                                did_change = true;
                                self.save_session_state();
                                if let Some(response) = response {
                                    let _ = response.send(RemoteActionResult::ok(None, None));
                                }
                                continue;
                            }
                            Err(error) => {
                                if let Some(response) = response {
                                    let _ = response.send(RemoteActionResult::error(error));
                                }
                                continue;
                            }
                        }
                    };

                    match self.process_manager.restart_ssh_session(
                        &mut self.state,
                        &tab_id,
                        dimensions,
                    ) {
                        Ok(_) => {
                            did_change = true;
                            self.save_session_state();
                            RemoteActionResult::ok(None, None)
                        }
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::DisconnectSsh { connection_id } => {
                    if let Some(tab_id) = self
                        .state
                        .find_ssh_tab_by_connection(&connection_id)
                        .map(|tab| tab.id.clone())
                    {
                        match self
                            .process_manager
                            .close_ssh_session(&mut self.state, &tab_id)
                        {
                            Ok(()) => {
                                did_change = true;
                                self.save_session_state();
                                RemoteActionResult::ok(None, None)
                            }
                            Err(error) => RemoteActionResult::error(error),
                        }
                    } else {
                        RemoteActionResult::ok(None, None)
                    }
                }
                RemoteAction::CloseTab { tab_id } => {
                    if let Some(tab) = self.state.find_tab(&tab_id).cloned() {
                        let result = match tab.tab_type {
                            TabType::Server => {
                                let command_id = tab.command_id.unwrap_or(tab.id.clone());
                                let _ = self.process_manager.stop_server(&command_id);
                                self.state.remove_tab(&tab_id);
                                Ok(())
                            }
                            TabType::Claude | TabType::Codex => self
                                .process_manager
                                .close_ai_session(&mut self.state, &tab_id),
                            TabType::Ssh => self
                                .process_manager
                                .close_ssh_session(&mut self.state, &tab_id),
                        };
                        match result {
                            Ok(()) => {
                                did_change = true;
                                self.synced_session_id = None;
                                self.last_dimensions = None;
                                self.save_session_state();
                                RemoteActionResult::ok(None, None)
                            }
                            Err(error) => RemoteActionResult::error(error),
                        }
                    } else {
                        RemoteActionResult::ok(None, None)
                    }
                }
                RemoteAction::SaveProject { project } => {
                    self.state.upsert_project(project);
                    did_change = true;
                    self.save_config_state();
                    self.save_session_state();
                    RemoteActionResult::ok(None, None)
                }
                RemoteAction::DeleteProject { project_id } => {
                    self.delete_project_action(&project_id, cx);
                    did_change = true;
                    RemoteActionResult::ok(None, None)
                }
                RemoteAction::SaveFolder {
                    project_id,
                    folder,
                    env_file_contents,
                } => {
                    if let Some(contents) = env_file_contents.as_ref() {
                        if let Some(env_file_path) = folder.env_file_path.as_ref() {
                            let env_path =
                                std::path::Path::new(&folder.folder_path).join(env_file_path);
                            if let Err(error) = env_service::write_env_text(&env_path, contents) {
                                if let Some(response) = response {
                                    let _ = response.send(RemoteActionResult::error(error));
                                }
                                continue;
                            }
                        }
                    }
                    if self.state.upsert_folder(&project_id, folder) {
                        did_change = true;
                        self.save_config_state();
                        self.save_session_state();
                        RemoteActionResult::ok(None, None)
                    } else {
                        RemoteActionResult::error("Could not save folder")
                    }
                }
                RemoteAction::DeleteFolder {
                    project_id,
                    folder_id,
                } => {
                    self.delete_folder_action(&project_id, &folder_id, cx);
                    did_change = true;
                    RemoteActionResult::ok(None, None)
                }
                RemoteAction::SaveCommand {
                    project_id,
                    folder_id,
                    command,
                } => {
                    if self.state.upsert_command(&project_id, &folder_id, command) {
                        did_change = true;
                        self.save_config_state();
                        self.save_session_state();
                        RemoteActionResult::ok(None, None)
                    } else {
                        RemoteActionResult::error("Could not save command")
                    }
                }
                RemoteAction::DeleteCommand {
                    project_id,
                    folder_id,
                    command_id,
                } => {
                    let _ = self.process_manager.stop_server(&command_id);
                    self.state
                        .remove_command(&project_id, &folder_id, &command_id);
                    did_change = true;
                    self.save_config_state();
                    self.save_session_state();
                    RemoteActionResult::ok(None, None)
                }
                RemoteAction::SaveSsh { connection } => {
                    self.state.upsert_ssh_connection(connection);
                    did_change = true;
                    self.save_config_state();
                    self.save_session_state();
                    RemoteActionResult::ok(None, None)
                }
                RemoteAction::DeleteSsh { connection_id } => {
                    self.delete_ssh_action(&connection_id, cx);
                    did_change = true;
                    RemoteActionResult::ok(None, None)
                }
                RemoteAction::SaveSettings { settings } => {
                    self.state.update_settings(settings);
                    self.process_manager
                        .set_log_buffer_size(self.state.settings().log_buffer_size as usize);
                    self.save_config_state();
                    did_change = true;
                    RemoteActionResult::ok(None, None)
                }
                RemoteAction::BrowsePath {
                    directories_only,
                    start_path,
                } => {
                    let _dialog_pause = self.pause_for_native_dialog();
                    RemoteActionResult::ok(
                        None,
                        Some(RemoteActionPayload::BrowsePath {
                            path: browse_remote_host_path(start_path.as_deref(), directories_only),
                        }),
                    )
                }
                RemoteAction::ListDirectory { path } => match list_remote_directory(&path) {
                    Ok(entries) => RemoteActionResult::ok(
                        None,
                        Some(RemoteActionPayload::DirectoryEntries { entries }),
                    ),
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::StatPath { path } => match remote_fs_entry_for_path(&path) {
                    Ok(entry) => {
                        RemoteActionResult::ok(None, Some(RemoteActionPayload::PathStat { entry }))
                    }
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::ReadTextFile { path } => match std::fs::read_to_string(&path) {
                    Ok(contents) => RemoteActionResult::ok(
                        None,
                        Some(RemoteActionPayload::TextFile { path, contents }),
                    ),
                    Err(error) => RemoteActionResult::error(format!(
                        "Could not read `{path}` on the host: {error}"
                    )),
                },
                RemoteAction::WriteTextFile { path, contents } => {
                    match std::fs::write(&path, contents) {
                        Ok(()) => RemoteActionResult::ok(None, None),
                        Err(error) => RemoteActionResult::error(format!(
                            "Could not write `{path}` on the host: {error}"
                        )),
                    }
                }
                RemoteAction::ScanRoot { root_path } => {
                    match scanner_service::scan_root(&root_path) {
                        Ok(entries) => RemoteActionResult::ok(
                            None,
                            Some(RemoteActionPayload::RootScan { entries }),
                        ),
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::ScanFolder { folder_path } => {
                    match scanner_service::scan_project(&folder_path) {
                        Ok(scan) => RemoteActionResult::ok(
                            None,
                            Some(RemoteActionPayload::FolderScan { scan }),
                        ),
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::SearchSession {
                    session_id,
                    query,
                    case_sensitive,
                } => {
                    let matches = self
                        .process_manager
                        .search_session(&session_id, &query, case_sensitive, 256)
                        .unwrap_or_default();
                    RemoteActionResult::ok(
                        None,
                        Some(RemoteActionPayload::SearchMatches { matches }),
                    )
                }
                RemoteAction::ScrollSessionToBufferLine {
                    session_id,
                    buffer_line,
                } => self
                    .process_manager
                    .scroll_session_to_buffer_line(&session_id, buffer_line)
                    .map(|_| RemoteActionResult::ok(None, None))
                    .unwrap_or_else(RemoteActionResult::error),
                RemoteAction::ScrollSessionToOffset {
                    session_id,
                    display_offset,
                } => self
                    .process_manager
                    .scroll_session_to_offset(&session_id, display_offset)
                    .map(|_| RemoteActionResult::ok(None, None))
                    .unwrap_or_else(RemoteActionResult::error),
                RemoteAction::ScrollSession {
                    session_id,
                    delta_lines,
                } => self
                    .process_manager
                    .scroll_session(&session_id, delta_lines)
                    .map(|_| RemoteActionResult::ok(None, None))
                    .unwrap_or_else(RemoteActionResult::error),
                RemoteAction::ExportSessionText { session_id, export } => {
                    let text = match export {
                        RemoteTerminalExport::Screen => {
                            self.process_manager.session_screen_text(&session_id)
                        }
                        RemoteTerminalExport::Scrollback => {
                            self.process_manager.session_scrollback_text(&session_id)
                        }
                        RemoteTerminalExport::Selection { text } => Ok(text),
                    };
                    match text {
                        Ok(text) if !text.is_empty() => RemoteActionResult::ok(
                            None,
                            Some(RemoteActionPayload::ExportText { text }),
                        ),
                        Ok(_) => RemoteActionResult::error("Nothing to export from this terminal."),
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::GitListRepos => RemoteActionResult::ok(
                    None,
                    Some(RemoteActionPayload::GitRepos {
                        repos: collect_git_repositories(&self.state),
                    }),
                ),
                RemoteAction::GitStatus { repo_path } => match git_service::status(&repo_path) {
                    Ok(status) => RemoteActionResult::ok(
                        None,
                        Some(RemoteActionPayload::GitStatus { status }),
                    ),
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::GitLog {
                    repo_path,
                    limit,
                    skip,
                } => match git_service::log(&repo_path, limit, skip) {
                    Ok(entries) => RemoteActionResult::ok(
                        None,
                        Some(RemoteActionPayload::GitLogEntries { entries }),
                    ),
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::GitDiffFile {
                    repo_path,
                    file_path,
                    staged,
                } => match git_service::diff_file(&repo_path, &file_path, staged) {
                    Ok(diff) => {
                        RemoteActionResult::ok(None, Some(RemoteActionPayload::GitDiff { diff }))
                    }
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::GitDiffCommit { repo_path, hash } => {
                    match git_service::diff_commit(&repo_path, &hash) {
                        Ok(diff) => RemoteActionResult::ok(
                            None,
                            Some(RemoteActionPayload::GitDiff { diff }),
                        ),
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::GitBranches { repo_path } => {
                    match git_service::branches(&repo_path) {
                        Ok(branches) => RemoteActionResult::ok(
                            None,
                            Some(RemoteActionPayload::GitBranches { branches }),
                        ),
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::GitStage { repo_path, files } => {
                    let file_refs = files.iter().map(|file| file.as_str()).collect::<Vec<_>>();
                    match git_service::stage(&repo_path, &file_refs) {
                        Ok(()) => RemoteActionResult::ok(None, None),
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::GitUnstage { repo_path, files } => {
                    let file_refs = files.iter().map(|file| file.as_str()).collect::<Vec<_>>();
                    match git_service::unstage(&repo_path, &file_refs) {
                        Ok(()) => RemoteActionResult::ok(None, None),
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::GitStageAll { repo_path } => match git_service::stage_all(&repo_path)
                {
                    Ok(()) => RemoteActionResult::ok(None, None),
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::GitUnstageAll { repo_path } => {
                    match git_service::unstage_all(&repo_path) {
                        Ok(()) => RemoteActionResult::ok(None, None),
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::GitCommit {
                    repo_path,
                    summary,
                    body,
                } => match git_service::commit(&repo_path, &summary, body.as_deref()) {
                    Ok(hash) => {
                        RemoteActionResult::ok(None, Some(RemoteActionPayload::GitCommit { hash }))
                    }
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::GitPush { repo_path } => match git_service::push(&repo_path) {
                    Ok(message) => RemoteActionResult::ok(Some(message), None),
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::GitPushSetUpstream { repo_path, branch } => {
                    match git_service::push_set_upstream(&repo_path, &branch) {
                        Ok(message) => RemoteActionResult::ok(Some(message), None),
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::GitPull { repo_path } => match git_service::pull(&repo_path) {
                    Ok(message) => RemoteActionResult::ok(Some(message), None),
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::GitFetch { repo_path } => match git_service::fetch(&repo_path) {
                    Ok(message) => RemoteActionResult::ok(Some(message), None),
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::GitSync { repo_path } => match git_service::sync(&repo_path) {
                    Ok(message) => RemoteActionResult::ok(Some(message), None),
                    Err(error) => RemoteActionResult::error(error),
                },
                RemoteAction::GitSwitchBranch { repo_path, name } => {
                    match git_service::switch_branch(&repo_path, &name) {
                        Ok(()) => RemoteActionResult::ok(None, None),
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::GitCreateBranch { repo_path, name } => {
                    match git_service::create_branch(&repo_path, &name) {
                        Ok(()) => RemoteActionResult::ok(None, None),
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::GitDeleteBranch { repo_path, name } => {
                    match git_service::delete_branch(&repo_path, &name) {
                        Ok(()) => RemoteActionResult::ok(None, None),
                        Err(error) => RemoteActionResult::error(error),
                    }
                }
                RemoteAction::GitGetGithubAuthStatus => {
                    let token = self.state.settings().github_token.clone();
                    if let Some(token) = token.filter(|token| !token.trim().is_empty()) {
                        let username = git_service::get_github_username(&token).ok();
                        RemoteActionResult::ok(
                            None,
                            Some(RemoteActionPayload::GitAuthStatus {
                                has_token: true,
                                username,
                            }),
                        )
                    } else {
                        RemoteActionResult::ok(
                            None,
                            Some(RemoteActionPayload::GitAuthStatus {
                                has_token: false,
                                username: None,
                            }),
                        )
                    }
                }
                RemoteAction::GitRequestDeviceCode => {
                    if let Some(client_id) = git_service::get_github_client_id() {
                        match git_service::request_device_code(&client_id) {
                            Ok(device_code) => RemoteActionResult::ok(
                                None,
                                Some(RemoteActionPayload::GitDeviceCode { device_code }),
                            ),
                            Err(error) => RemoteActionResult::error(error),
                        }
                    } else {
                        RemoteActionResult::error(
                            "Set DEVMANAGER_GITHUB_CLIENT_ID on the host, or use the built-in default GitHub OAuth app.",
                        )
                    }
                }
                RemoteAction::GitPollForToken { device_code } => {
                    if let Some(client_id) = git_service::get_github_client_id() {
                        match git_service::poll_for_token(&client_id, &device_code) {
                            Ok(Some(token_resp)) => {
                                let username =
                                    git_service::get_github_username(&token_resp.access_token).ok();
                                let mut settings = self.state.settings().clone();
                                settings.github_token = Some(token_resp.access_token);
                                self.state.update_settings(settings);
                                self.save_config_state();
                                did_change = true;
                                RemoteActionResult::ok(
                                    None,
                                    Some(RemoteActionPayload::GitTokenPoll {
                                        completed: true,
                                        username,
                                    }),
                                )
                            }
                            Ok(None) => RemoteActionResult::ok(
                                None,
                                Some(RemoteActionPayload::GitTokenPoll {
                                    completed: false,
                                    username: None,
                                }),
                            ),
                            Err(error) => RemoteActionResult::error(error),
                        }
                    } else {
                        RemoteActionResult::error(
                            "Set DEVMANAGER_GITHUB_CLIENT_ID on the host, or use the built-in default GitHub OAuth app.",
                        )
                    }
                }
                RemoteAction::GitLogout => {
                    let mut settings = self.state.settings().clone();
                    settings.github_token = None;
                    self.state.update_settings(settings);
                    self.save_config_state();
                    did_change = true;
                    RemoteActionResult::ok(None, None)
                }
                RemoteAction::GitGenerateCommitMessage { repo_path } => {
                    if let Some(token) = self
                        .state
                        .settings()
                        .github_token
                        .clone()
                        .filter(|token| !token.trim().is_empty())
                    {
                        match git_service::get_staged_diff(&repo_path) {
                            Ok(diff) if diff.trim().is_empty() => {
                                RemoteActionResult::error("No staged changes to summarize")
                            }
                            Ok(diff) => match git_service::generate_commit_message(&token, &diff) {
                                Ok(message) => RemoteActionResult::ok(
                                    None,
                                    Some(RemoteActionPayload::GitCommitMessage { message }),
                                ),
                                Err(error) => RemoteActionResult::error(format!("AI: {error}")),
                            },
                            Err(error) => RemoteActionResult::error(error),
                        }
                    } else {
                        RemoteActionResult::error("GitHub token not configured on the host.")
                    }
                }
            };

            if let Some(response) = response {
                let _ = response.send(result);
            }
        }

        if did_change {
            self.last_remote_snapshot_sync_at = None;
            cx.notify();
        }
        did_change
    }

    fn perform_managed_shutdown(&mut self) -> ManagedShutdownReport {
        self.save_session_state();
        self.process_manager
            .shutdown_managed_processes(APP_SHUTDOWN_TIMEOUT)
    }

    fn handle_window_should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.sync_terminal_focus(None);
        self.capture_window_bounds(window);

        let keep_hosting_in_background = self.remote_mode.is_none()
            && self.remote_machine_state.host.enabled
            && self.remote_machine_state.host.keep_hosting_in_background;
        if keep_hosting_in_background {
            self.save_session_state();
            window.minimize_window();
            self.terminal_notice = Some(
                "DevManager is keeping remote hosting alive in the background. Reopen it from the taskbar to return to the window."
                    .to_string(),
            );
            self.set_remote_status_notice(
                "Hosting remains alive in the background for remote clients.",
                false,
            );
            self.sync_settings_remote_draft();
            cx.notify();
            return false;
        }

        if self.state.settings().minimize_to_tray {
            self.save_session_state();
            window.minimize_window();
            self.terminal_notice = Some(
                "DevManager minimized instead of closing. Disable `Minimize instead of close` to quit from the window close button."
                    .to_string(),
            );
            cx.notify();
            return false;
        }

        let live_sessions = self.process_manager.live_session_count();
        if self.state.settings().confirm_on_close && live_sessions > 0 {
            let _dialog_pause = self.pause_for_native_dialog();
            let result = MessageDialog::new()
                .set_level(MessageLevel::Warning)
                .set_title("Quit DevManager?")
                .set_description(format!(
                    "DevManager still has {live_sessions} live session(s). Quitting now will stop them."
                ))
                .set_buttons(MessageButtons::YesNo)
                .show();
            if result != MessageDialogResult::Yes {
                self.save_session_state();
                self.terminal_notice = Some("Quit canceled.".to_string());
                cx.notify();
                return false;
            }
        }

        self.save_session_state();
        self.perform_managed_shutdown();
        true
    }

    fn preview_notification_sound_action(&mut self, cx: &mut Context<Self>) {
        let sound_id = self.state.settings().notification_sound.as_deref();
        notifications::play_notification_sound(sound_id);
        self.editor_notice = Some(match sound_id {
            Some(sound_id) if sound_id.eq_ignore_ascii_case("none") => {
                "Notification sound is disabled.".to_string()
            }
            Some(sound_id) => format!("Previewed `{sound_id}`."),
            None => "Previewed `glass`.".to_string(),
        });
        cx.notify();
    }

    fn sidebar_width(&self) -> f32 {
        sidebar::sidebar_width_px(self.state.sidebar_collapsed)
    }

    fn terminal_dimensions(&self, window: &Window) -> SessionDimensions {
        let Some(layout) = self.terminal_viewport_layout(window, false) else {
            return SessionDimensions::default();
        };
        let metrics = self.terminal_render_metrics(window);
        let available_width = if self.state.settings().show_terminal_scrollbar {
            (layout.available_width - view::TERMINAL_SCROLLBAR_WIDTH_PX).max(metrics.cell_width)
        } else {
            layout.available_width
        };

        SessionDimensions::from_available_space(
            available_width,
            layout.available_height,
            metrics.cell_width,
            metrics.line_height,
        )
    }

    fn toggle_sidebar_action(&mut self, cx: &mut Context<Self>) {
        self.state.toggle_sidebar();
        self.last_dimensions = None;
        self.save_session_state();
        cx.notify();
    }

    fn stop_all_servers_action(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            let command_ids: Vec<String> = self
                .current_runtime_snapshot()
                .sessions
                .iter()
                .filter(|(_, session)| {
                    matches!(session.session_kind, crate::state::SessionKind::Server)
                        && session.status.is_live()
                })
                .map(|(command_id, _)| command_id.clone())
                .collect();

            for command_id in &command_ids {
                self.remote_send_action(RemoteAction::StopServer {
                    command_id: command_id.clone(),
                });
            }

            self.terminal_notice = Some(if command_ids.is_empty() {
                "No running remote servers to stop.".to_string()
            } else {
                format!("Stopping {} remote server tab(s).", command_ids.len())
            });
            cx.notify();
            return;
        }

        let stopped = self.process_manager.stop_all_servers();
        self.terminal_notice = Some(if stopped == 0 {
            "No running servers to stop.".to_string()
        } else {
            format!("Stopping {stopped} running server tab(s).")
        });
        cx.notify();
    }

    fn close_tab_action(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        let Some(tab) = self.state.find_tab(tab_id).cloned() else {
            return;
        };
        if self.should_confirm_tab_close(&tab) && !self.confirm_live_tab_close(&tab) {
            self.terminal_notice = Some("Tab close canceled.".to_string());
            cx.notify();
            return;
        }

        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            match self.remote_request(RemoteAction::CloseTab {
                tab_id: tab_id.to_string(),
            }) {
                Ok(result) if result.ok => {
                    self.state.remove_tab(tab_id);
                    self.synced_session_id = None;
                    self.last_dimensions = None;
                    self.terminal_notice = None;
                }
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Failed to close remote tab.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Failed to close remote tab: {error}"));
                }
            }
            cx.notify();
            return;
        }

        match tab.tab_type {
            TabType::Server => {
                let _ = self.process_manager.stop_server(tab_id);
                self.state.remove_tab(tab_id);
                self.synced_session_id = None;
                self.last_dimensions = None;
                self.save_session_state();
                cx.notify();
            }
            TabType::Claude | TabType::Codex => {
                self.close_ai_tab_action(tab_id, cx);
            }
            TabType::Ssh => {
                let _ = self
                    .process_manager
                    .close_ssh_session(&mut self.state, tab_id);
                self.state.remove_tab(tab_id);
                self.synced_session_id = None;
                self.last_dimensions = None;
                self.save_session_state();
                cx.notify();
            }
        }
    }

    fn confirm_live_tab_close(&self, tab: &crate::models::SessionTab) -> bool {
        let label = self.state.tab_label(tab);
        let description = match tab.tab_type {
            TabType::Server => format!("Close `{label}` and stop its running server?"),
            TabType::Claude | TabType::Codex => {
                format!("Close `{label}` and stop its live AI session?")
            }
            TabType::Ssh => format!("Close `{label}` and disconnect its live SSH session?"),
        };

        let _dialog_pause = self.pause_for_native_dialog();
        MessageDialog::new()
            .set_level(MessageLevel::Warning)
            .set_title("Confirm Close")
            .set_description(description)
            .set_buttons(MessageButtons::YesNo)
            .show()
            == MessageDialogResult::Yes
    }

    fn should_confirm_tab_close(&self, tab: &crate::models::SessionTab) -> bool {
        if !self.state.settings().confirm_on_close {
            return false;
        }
        let runtime = self.current_runtime_snapshot();

        match tab.tab_type {
            TabType::Server => tab
                .command_id
                .as_deref()
                .and_then(|command_id| runtime.sessions.get(command_id))
                .map(|session| session.status.is_live())
                .unwrap_or(false),
            TabType::Claude | TabType::Codex | TabType::Ssh => tab
                .pty_session_id
                .as_deref()
                .and_then(|session_id| runtime.sessions.get(session_id))
                .map(|session| session.status.is_live())
                .unwrap_or(false),
        }
    }

    fn export_config_action(&mut self, cx: &mut Context<Self>) {
        let _dialog_pause = self.pause_for_native_dialog();
        match self
            .session_manager
            .export_config_dialog(&self.state.config)
        {
            Ok(Some(path)) => {
                self.editor_notice = Some(format!("Exported config to {}", path.display()));
            }
            Ok(None) => {}
            Err(error) => {
                self.editor_notice = Some(format!("Failed to export config: {error}"));
            }
        }
        cx.notify();
    }

    fn import_config_action(&mut self, mode: ConfigImportMode, cx: &mut Context<Self>) {
        let _dialog_pause = self.pause_for_native_dialog();
        match self
            .session_manager
            .import_config_dialog(&self.state.config, mode)
        {
            Ok(Some((config, path))) => {
                self.apply_imported_config(config, mode, &path, cx);
            }
            Ok(None) => {}
            Err(error) => {
                self.editor_notice = Some(format!("Failed to import config: {error}"));
                cx.notify();
            }
        }
    }

    fn apply_imported_config(
        &mut self,
        config: AppConfig,
        mode: ConfigImportMode,
        source_path: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        let removed_ai_tabs: Vec<String> = self
            .state
            .ai_tabs()
            .filter(|tab| !config_has_project(&config, &tab.project_id))
            .map(|tab| tab.id.clone())
            .collect();
        let removed_server_tabs: Vec<String> = self
            .state
            .open_tabs
            .iter()
            .filter(|tab| {
                matches!(tab.tab_type, TabType::Server)
                    && tab
                        .command_id
                        .as_deref()
                        .is_some_and(|command_id| !config_has_command(&config, command_id))
            })
            .map(|tab| tab.id.clone())
            .collect();
        let removed_ssh_tabs: Vec<String> = self
            .state
            .ssh_tabs()
            .filter(|tab| {
                !config_has_project(&config, &tab.project_id)
                    || tab
                        .ssh_connection_id
                        .as_deref()
                        .is_some_and(|connection_id| {
                            !config_has_ssh_connection(&config, connection_id)
                        })
            })
            .map(|tab| tab.id.clone())
            .collect();

        self.state.config = config;

        for tab_id in removed_ai_tabs {
            let _ = self
                .process_manager
                .close_ai_session(&mut self.state, &tab_id);
        }
        for tab_id in removed_server_tabs {
            let _ = self.process_manager.stop_server(&tab_id);
            self.state.remove_tab(&tab_id);
        }
        for tab_id in removed_ssh_tabs {
            let _ = self
                .process_manager
                .close_ssh_session(&mut self.state, &tab_id);
            self.state.remove_tab(&tab_id);
        }

        self.synced_session_id = None;
        self.last_dimensions = None;
        self.save_config_state();
        self.save_session_state();

        if matches!(self.editor_panel, Some(EditorPanel::Settings(_))) {
            self.open_settings_action(cx);
        }

        let mode_label = match mode {
            ConfigImportMode::Merge => "Imported config with merge",
            ConfigImportMode::Replace => "Replaced config from import",
        };
        self.editor_notice = Some(format!("{mode_label}: {}", source_path.display()));
        cx.notify();
    }

    fn check_for_updates_action(&mut self, cx: &mut Context<Self>) {
        match self.updater.check_for_updates() {
            Ok(()) => {
                self.editor_notice = Some("Checking for updates...".to_string());
            }
            Err(error) => {
                self.editor_notice = Some(error);
            }
        }
        cx.notify();
    }

    fn download_update_action(&mut self, cx: &mut Context<Self>) {
        match self.updater.download_update() {
            Ok(()) => {
                self.editor_notice = Some("Downloading the latest update...".to_string());
            }
            Err(error) => {
                self.editor_notice = Some(error);
            }
        }
        cx.notify();
    }

    fn install_update_action(&mut self, cx: &mut Context<Self>) {
        let live_sessions = self.process_manager.live_session_count();
        match self.updater.install_update() {
            Ok(version) => {
                let shutdown = self.perform_managed_shutdown();
                let closed_sessions = shutdown.requested_sessions;
                self.editor_notice = Some(if closed_sessions > 0 {
                    format!(
                        "Installer for {version} launched. Closed {closed_sessions} live session(s) before exit."
                    )
                } else if live_sessions > 0 {
                    format!(
                        "Installer for {version} launched. DevManager is closing to finish the update."
                    )
                } else {
                    format!("Installer for {version} launched. DevManager is closing.")
                });
                cx.notify();
                std::process::exit(0);
            }
            Err(error) => {
                self.editor_notice = Some(error);
                cx.notify();
            }
        }
    }

    fn terminal_font_size(&self) -> f32 {
        self.state
            .settings()
            .terminal_font_size
            .map(|value| value as f32)
            .unwrap_or(view::TERMINAL_FONT_SIZE)
            .clamp(8.0, 32.0)
    }

    fn terminal_render_metrics(&self, window: &Window) -> TerminalRenderMetrics {
        let font_size = self.terminal_font_size();
        let font_size_px = px(font_size);
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&terminal::terminal_font());
        let cell_width = text_system
            .ch_advance(font_id, font_size_px)
            .map(f32::from)
            .or_else(|_| text_system.ch_width(font_id, font_size_px).map(f32::from))
            .unwrap_or(8.0)
            .max(6.0);
        let ascent = f32::from(text_system.ascent(font_id, font_size_px));
        let descent = f32::from(text_system.descent(font_id, font_size_px)).abs();
        let line_height = (ascent + descent + 1.0).ceil().max(font_size + 1.0);

        TerminalRenderMetrics {
            cell_width,
            line_height,
        }
    }

    fn open_editor(&mut self, panel: EditorPanel, cx: &mut Context<Self>) {
        self.editor_panel = Some(panel);
        self.editor_active_field = None;
        self.editor_cursor = 0;
        self.editor_selection_anchor = None;
        self.is_selecting_editor = false;
        self.editor_notice = None;
        self.editor_needs_focus = true;
        self.did_focus_terminal = false;
        cx.notify();
    }

    fn open_editor_with_field(
        &mut self,
        panel: EditorPanel,
        field: EditorField,
        cx: &mut Context<Self>,
    ) {
        let cursor = panel
            .text_value(field)
            .map(|value| value.chars().count())
            .unwrap_or(0);
        self.open_editor(panel, cx);
        self.editor_active_field = Some(field);
        self.editor_cursor = cursor;
        cx.notify();
    }

    fn close_editor(&mut self, cx: &mut Context<Self>) {
        self.show_terminal_surface();
        cx.notify();
    }

    fn show_terminal_surface(&mut self) {
        self.editor_panel = None;
        self.editor_active_field = None;
        self.editor_cursor = 0;
        self.editor_selection_anchor = None;
        self.is_selecting_editor = false;
        self.editor_notice = None;
        self.editor_needs_focus = false;
        self.did_focus_terminal = false;
    }

    fn focus_editor(&mut self, window: &mut Window) {
        self.sync_terminal_focus(None);
        window.focus(&self.editor_focus);
        self.editor_needs_focus = false;
    }

    fn open_settings_action(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.editor_panel,
            Some(EditorPanel::Settings(SettingsDraft {
                remote_focus_only: false,
                ..
            }))
        ) {
            self.close_editor(cx);
            return;
        }
        self.open_settings_panel(false, cx);
    }

    fn open_settings_panel(&mut self, remote_focus_only: bool, cx: &mut Context<Self>) {
        let settings = self.state.settings().clone();
        let remote_status = self.remote_host_service.status();
        let preferred_known_host = self.preferred_known_remote_host();
        let remote_connected_label = self
            .remote_mode
            .as_ref()
            .map(|remote_mode| remote_mode.connected_label.clone());
        let remote_connected_endpoint = self
            .remote_mode
            .as_ref()
            .map(|remote_mode| format!("{}:{}", remote_mode.address, remote_mode.port));
        let remote_connected_server_id = self
            .remote_mode
            .as_ref()
            .map(|remote_mode| remote_mode.client.server_id().to_string());
        let remote_connected_fingerprint = self
            .remote_mode
            .as_ref()
            .map(|remote_mode| remote_mode.client.certificate_fingerprint().to_string());
        let (remote_reconnect_attempts, remote_reconnect_last_error) = self
            .remote_mode
            .as_ref()
            .and_then(|remote_mode| remote_mode.reconnect.as_ref())
            .map(|reconnect| (reconnect.attempts, reconnect.last_error.clone()))
            .unwrap_or((0, None));
        let remote_latency_summary = self.remote_mode.as_ref().and_then(|remote_mode| {
            format_remote_latency_summary(&remote_mode.client.latency_stats())
        });
        let remote_port_forwards = self.remote_port_forward_rows();
        let remote_host_latency_summary = format_remote_latency_summary(&remote_status.latency);
        self.open_editor(
            EditorPanel::Settings(SettingsDraft {
                remote_focus_only,
                default_terminal: settings.default_terminal,
                mac_terminal_profile: settings.mac_terminal_profile.unwrap_or_default(),
                theme: settings.theme,
                log_buffer_size: settings.log_buffer_size.to_string(),
                claude_command: settings.claude_command.unwrap_or_default(),
                codex_command: settings.codex_command.unwrap_or_default(),
                notification_sound: settings
                    .notification_sound
                    .unwrap_or_else(|| "glass".to_string()),
                confirm_on_close: settings.confirm_on_close,
                minimize_to_tray: settings.minimize_to_tray,
                restore_session_on_start: settings.restore_session_on_start.unwrap_or(true),
                terminal_font_size: settings
                    .terminal_font_size
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                option_as_meta: settings.option_as_meta,
                copy_on_select: settings.copy_on_select,
                keep_selection_on_copy: settings.keep_selection_on_copy,
                show_terminal_scrollbar: settings.show_terminal_scrollbar,
                shell_integration_enabled: settings.shell_integration_enabled,
                terminal_mouse_override: settings.terminal_mouse_override,
                terminal_read_only: settings.terminal_read_only,
                github_token: settings.github_token.unwrap_or_default(),
                remote_host_enabled: self.remote_machine_state.host.enabled,
                remote_bind_address: self.remote_machine_state.host.bind_address.clone(),
                remote_port: self.remote_machine_state.host.port.to_string(),
                remote_keep_hosting_in_background: self
                    .remote_machine_state
                    .host
                    .keep_hosting_in_background,
                remote_pairing_token: remote_status.pairing_token,
                remote_connect_address: preferred_known_host
                    .as_ref()
                    .map(|host| host.address.clone())
                    .unwrap_or_default(),
                remote_connect_port: preferred_known_host
                    .as_ref()
                    .map(|host| host.port.to_string())
                    .unwrap_or_else(|| "43871".to_string()),
                remote_connect_token: String::new(),
                remote_connect_in_flight: false,
                remote_connect_status: self
                    .remote_status_notice
                    .as_ref()
                    .map(|notice| notice.message.clone()),
                remote_connect_status_is_error: self
                    .remote_status_notice
                    .as_ref()
                    .map(|notice| notice.is_error)
                    .unwrap_or(false),
                remote_connected_label,
                remote_connected_endpoint,
                remote_connected_server_id,
                remote_connected_fingerprint,
                remote_latency_summary,
                remote_reconnect_attempts,
                remote_reconnect_last_error,
                remote_has_control: self.remote_has_control(),
                remote_connected: self.remote_mode.is_some(),
                remote_host_clients: remote_status.connected_clients,
                remote_host_controller_client_id: remote_status.controller_client_id,
                remote_host_listening: remote_status.listening,
                remote_host_error: remote_status.listener_error,
                remote_host_last_note: remote_status.last_connection_note,
                remote_host_last_note_is_error: remote_status.last_connection_is_error,
                remote_host_latency_summary,
                remote_host_server_id: self.remote_machine_state.host.server_id.clone(),
                remote_host_fingerprint: self
                    .remote_machine_state
                    .host
                    .certificate_fingerprint
                    .clone(),
                remote_port_forwards,
                remote_known_hosts: self.remote_machine_state.known_hosts.clone(),
                remote_paired_clients: self.remote_machine_state.host.paired_clients.clone(),
                open_picker: None,
            }),
            cx,
        );
    }

    fn open_git_window(&mut self, cx: &mut Context<Self>) {
        let remote_client = self
            .remote_mode
            .as_ref()
            .map(|remote_mode| remote_mode.client.clone());
        if let Some(client) = remote_client {
            self.editor_notice = Some("Loading remote Git repositories...".to_string());
            cx.notify();
            let open_client = client.clone();
            cx.spawn(
                move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                    let mut cx = cx.clone();
                    async move {
                        let result = cx
                            .background_executor()
                            .spawn(async move { client.request(RemoteAction::GitListRepos) })
                            .await;
                        let _ = this.update(&mut cx, |this, cx| match result {
                            Ok(result) if result.ok => match result.payload {
                                Some(RemoteActionPayload::GitRepos { repos }) => {
                                    let repos = repos
                                        .into_iter()
                                        .map(|repo| (repo.label, repo.path))
                                        .collect::<Vec<_>>();
                                    this.open_git_window_with_repos(
                                        repos,
                                        Some(open_client.clone()),
                                        cx,
                                    );
                                }
                                _ => {
                                    this.editor_notice = Some(
                                        "Remote host did not return any Git repositories."
                                            .to_string(),
                                    );
                                    cx.notify();
                                }
                            },
                            Ok(result) => {
                                this.editor_notice = Some(result.message.unwrap_or_else(|| {
                                    "Could not load remote Git repositories.".to_string()
                                }));
                                cx.notify();
                            }
                            Err(error) => {
                                this.editor_notice = Some(format!(
                                    "Could not load remote Git repositories: {error}"
                                ));
                                cx.notify();
                            }
                        });
                    }
                },
            )
            .detach();
            return;
        }

        let repos = collect_git_repositories(&self.state)
            .into_iter()
            .map(|repo| (repo.label, repo.path))
            .collect::<Vec<_>>();

        self.open_git_window_with_repos(repos, None, cx);
    }

    fn open_git_window_with_repos(
        &mut self,
        repos: Vec<(String, String)>,
        remote_client: Option<RemoteClientHandle>,
        cx: &mut Context<Self>,
    ) {
        if repos.is_empty() {
            self.editor_notice = Some("No Git repositories were found.".to_string());
            cx.notify();
            return;
        }

        self.editor_notice = None;
        let bounds = gpui::Bounds::centered(None, gpui::size(px(1024.0), px(700.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Git".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| {
                let repos = repos.clone();
                let remote_client = remote_client.clone();
                cx.new(move |cx| {
                    if let Some(client) = remote_client.clone() {
                        crate::git::GitWindow::new_remote(repos, client, cx)
                    } else {
                        crate::git::GitWindow::new(repos, cx)
                    }
                })
            },
        )
        .ok();
    }

    fn open_ui_preview_action(&mut self, cx: &mut Context<Self>) {
        self.open_editor(EditorPanel::UiPreview(UiPreviewDraft), cx);
    }

    fn open_add_project_action(&mut self, cx: &mut Context<Self>) {
        self.add_project_wizard = Some(workspace::AddProjectWizard::default());
        cx.notify();
    }

    fn wizard_create_action(&mut self, cx: &mut Context<Self>) {
        let Some(wizard) = self.add_project_wizard.as_ref() else {
            return;
        };
        if wizard.root_path.trim().is_empty() {
            self.editor_notice = Some("Project root path is required".to_string());
            cx.notify();
            return;
        }

        let Some(wizard) = self.add_project_wizard.take() else {
            return;
        };
        let project = build_project_from_wizard(wizard);
        let project_name = project.name.clone();

        if !self.ensure_mutation_control(cx) {
            self.add_project_wizard = None;
            return;
        }

        if self.remote_mode.is_some() {
            match self.remote_request(RemoteAction::SaveProject { project }) {
                Ok(result) if result.ok => {
                    if self.editor_notice.is_none() {
                        self.editor_notice = Some(format!("Created project `{project_name}`"));
                    }
                }
                Ok(result) => {
                    self.editor_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Could not create remote project.".to_string()),
                    );
                }
                Err(error) => {
                    self.editor_notice = Some(format!("Could not create remote project: {error}"));
                }
            }
            cx.notify();
            return;
        }

        self.state.upsert_project(project);
        self.save_config_state();
        self.save_session_state();
        if self.editor_notice.is_none() {
            self.editor_notice = Some(format!("Created project `{project_name}`"));
        }
        cx.notify();
    }

    fn wizard_pick_root_folder(&mut self, cx: &mut Context<Self>) {
        if self.remote_mode.is_some() {
            let picked_path = match self.remote_request(RemoteAction::BrowsePath {
                directories_only: true,
                start_path: None,
            }) {
                Ok(RemoteActionResult {
                    ok: true,
                    payload: Some(RemoteActionPayload::BrowsePath { path }),
                    ..
                }) => path,
                Ok(result) => {
                    self.editor_notice = Some(result.message.unwrap_or_else(|| {
                        "Could not pick a folder on the remote host.".to_string()
                    }));
                    cx.notify();
                    return;
                }
                Err(error) => {
                    self.editor_notice =
                        Some(format!("Could not open the remote host picker: {error}"));
                    cx.notify();
                    return;
                }
            };
            let Some(root_path) = picked_path else {
                return;
            };
            let default_name = last_path_segment(&root_path);
            let scan_result = self.remote_request(RemoteAction::ScanRoot {
                root_path: root_path.clone(),
            });

            if let Some(wizard) = self.add_project_wizard.as_mut() {
                wizard.root_path = root_path.clone();
                if wizard.name.trim().is_empty() {
                    wizard.name = default_name;
                    wizard.cursor = wizard.name.len();
                }
                wizard.selected_scripts.clear();
                wizard.selected_port_variables.clear();

                match scan_result {
                    Ok(RemoteActionResult {
                        ok: true,
                        payload: Some(RemoteActionPayload::RootScan { entries }),
                        ..
                    }) => apply_root_scan_entries(wizard, entries),
                    Ok(result) => {
                        clear_root_scan_entries(
                            wizard,
                            result.message.unwrap_or_else(|| {
                                "Could not scan the selected remote project root.".to_string()
                            }),
                        );
                    }
                    Err(error) => {
                        clear_root_scan_entries(
                            wizard,
                            format!("Could not scan the selected remote project root: {error}"),
                        );
                    }
                }
                cx.notify();
            }
            return;
        }

        let _dialog_pause = self.pause_for_native_dialog();
        let Some(path) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let root_path = path.to_string_lossy().to_string();
        let default_name = last_path_segment(&root_path);

        if let Some(wizard) = self.add_project_wizard.as_mut() {
            wizard.root_path = root_path;
            if wizard.name.trim().is_empty() {
                wizard.name = default_name;
                wizard.cursor = wizard.name.len();
            }
            wizard.selected_scripts.clear();
            wizard.selected_port_variables.clear();

            match scanner_service::scan_root(&wizard.root_path) {
                Ok(scan_entries) => apply_root_scan_entries(wizard, scan_entries),
                Err(error) => clear_root_scan_entries(wizard, error),
            }
            cx.notify();
        }
    }

    fn open_edit_project_action(&mut self, project_id: &str, cx: &mut Context<Self>) {
        if let Some(project) = self.state.find_project(project_id).cloned() {
            self.open_editor(
                EditorPanel::Project(ProjectDraft {
                    existing_id: Some(project.id),
                    name: project.name,
                    root_path: project.root_path,
                    color: project.color.unwrap_or_default(),
                    pinned: project.pinned.unwrap_or(false),
                    save_log_files: project.save_log_files.unwrap_or(true),
                    notes: project.notes.unwrap_or_default(),
                }),
                cx,
            );
        } else {
            self.editor_notice = Some(format!("Unknown project `{project_id}`"));
            cx.notify();
        }
    }

    fn open_project_notes_action(&mut self, project_id: &str, cx: &mut Context<Self>) {
        if let Some(project) = self.state.find_project(project_id).cloned() {
            self.open_editor_with_field(
                EditorPanel::Project(ProjectDraft {
                    existing_id: Some(project.id),
                    name: project.name,
                    root_path: project.root_path,
                    color: project.color.unwrap_or_default(),
                    pinned: project.pinned.unwrap_or(false),
                    save_log_files: project.save_log_files.unwrap_or(true),
                    notes: project.notes.unwrap_or_default(),
                }),
                EditorField::Project(workspace::ProjectField::Notes),
                cx,
            );
        } else {
            self.editor_notice = Some(format!("Unknown project `{project_id}`"));
            cx.notify();
        }
    }

    fn open_add_folder_action(&mut self, project_id: &str, cx: &mut Context<Self>) {
        let root_path = self
            .state
            .find_project(project_id)
            .map(|project| project.root_path.clone())
            .unwrap_or_default();
        let default_env_file_path =
            scanner_service::default_env_file_for_dir(std::path::Path::new(&root_path))
                .unwrap_or_default();
        let env_file_contents = if default_env_file_path.is_empty() {
            None
        } else {
            load_folder_env_contents(&root_path, &default_env_file_path)
        };
        let env_file_loaded = env_file_contents.is_some();
        let (git_branch, dependency_status) = inspect_folder_runtime_metadata(&root_path);
        self.open_editor(
            EditorPanel::Folder(FolderDraft {
                project_id: project_id.to_string(),
                existing_id: None,
                name: String::new(),
                folder_path: root_path,
                env_file_path: default_env_file_path,
                env_file_contents: env_file_contents.unwrap_or_default(),
                env_file_loaded,
                port_variable: String::new(),
                hidden: false,
                git_branch,
                dependency_status,
                scan_result: None,
                selected_scanned_scripts: Default::default(),
                selected_scanned_port_variable: None,
                scan_message: None,
                is_scanning: false,
            }),
            cx,
        );
    }

    fn open_edit_folder_action(
        &mut self,
        project_id: &str,
        folder_id: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(lookup) = self.state.find_folder(project_id, folder_id) {
            let env_file_path = lookup.folder.env_file_path.clone().unwrap_or_default();
            let env_file_contents = if env_file_path.is_empty() {
                None
            } else {
                load_folder_env_contents(&lookup.folder.folder_path, &env_file_path)
            };
            let (git_branch, dependency_status) =
                inspect_folder_runtime_metadata(&lookup.folder.folder_path);
            self.open_editor(
                EditorPanel::Folder(FolderDraft {
                    project_id: lookup.project.id.clone(),
                    existing_id: Some(lookup.folder.id.clone()),
                    name: lookup.folder.name.clone(),
                    folder_path: lookup.folder.folder_path.clone(),
                    env_file_path,
                    env_file_contents: env_file_contents.clone().unwrap_or_default(),
                    env_file_loaded: env_file_contents.is_some(),
                    port_variable: lookup.folder.port_variable.clone().unwrap_or_default(),
                    hidden: lookup.folder.hidden.unwrap_or(false),
                    git_branch,
                    dependency_status,
                    scan_result: None,
                    selected_scanned_scripts: Default::default(),
                    selected_scanned_port_variable: lookup.folder.port_variable.clone(),
                    scan_message: None,
                    is_scanning: false,
                }),
                cx,
            );
        } else {
            self.editor_notice = Some(format!("Unknown folder `{folder_id}`"));
            cx.notify();
        }
    }

    fn pick_folder_path_action(&mut self, cx: &mut Context<Self>) {
        if self.remote_mode.is_some() {
            let env_file_path = match self.editor_panel.as_ref() {
                Some(EditorPanel::Folder(draft)) => draft.env_file_path.trim().to_string(),
                _ => return,
            };
            let picked_path = match self.remote_request(RemoteAction::BrowsePath {
                directories_only: true,
                start_path: None,
            }) {
                Ok(RemoteActionResult {
                    ok: true,
                    payload: Some(RemoteActionPayload::BrowsePath { path }),
                    ..
                }) => path,
                Ok(result) => {
                    self.editor_notice = Some(result.message.unwrap_or_else(|| {
                        "Could not pick a folder on the remote host.".to_string()
                    }));
                    cx.notify();
                    return;
                }
                Err(error) => {
                    self.editor_notice =
                        Some(format!("Could not open the remote host picker: {error}"));
                    cx.notify();
                    return;
                }
            };
            let Some(folder_path) = picked_path else {
                return;
            };
            let default_name = last_path_segment(&folder_path);
            let env_contents = if env_file_path.is_empty() {
                None
            } else {
                match self.remote_request(RemoteAction::ReadTextFile {
                    path: std::path::Path::new(&folder_path)
                        .join(&env_file_path)
                        .to_string_lossy()
                        .to_string(),
                }) {
                    Ok(RemoteActionResult {
                        ok: true,
                        payload: Some(RemoteActionPayload::TextFile { contents, .. }),
                        ..
                    }) => Some(contents),
                    _ => None,
                }
            };

            if let Some(EditorPanel::Folder(draft)) = self.editor_panel.as_mut() {
                draft.folder_path = folder_path.clone();
                if draft.name.trim().is_empty() {
                    draft.name = default_name;
                }
                draft.git_branch = None;
                draft.dependency_status = None;
                draft.scan_result = None;
                draft.selected_scanned_scripts.clear();
                draft.selected_scanned_port_variable = None;
                if !env_file_path.is_empty() {
                    draft.env_file_loaded = env_contents.is_some();
                    draft.env_file_contents = env_contents.unwrap_or_default();
                } else {
                    draft.env_file_contents.clear();
                    draft.env_file_loaded = false;
                }
                draft.scan_message = Some(format!(
                    "Picked remote folder `{folder_path}`. Scan the folder to refresh scripts, ports, and repo status."
                ));
                cx.notify();
            }
            return;
        }

        let _dialog_pause = self.pause_for_native_dialog();
        let Some(path) = FileDialog::new().pick_folder() else {
            return;
        };
        let folder_path = path.to_string_lossy().to_string();
        let default_name = last_path_segment(&folder_path);

        if let Some(EditorPanel::Folder(draft)) = self.editor_panel.as_mut() {
            draft.folder_path = folder_path.clone();
            if draft.name.trim().is_empty() {
                draft.name = default_name;
            }
            if draft.env_file_path.trim().is_empty() {
                draft.env_file_path =
                    scanner_service::default_env_file_for_dir(std::path::Path::new(&folder_path))
                        .unwrap_or_default();
            }
            let (git_branch, dependency_status) = inspect_folder_runtime_metadata(&folder_path);
            draft.git_branch = git_branch;
            draft.dependency_status = dependency_status;
            if !draft.env_file_path.trim().is_empty() {
                draft.env_file_contents =
                    load_folder_env_contents(&folder_path, &draft.env_file_path)
                        .unwrap_or_default();
                draft.env_file_loaded = true;
            } else {
                draft.env_file_contents.clear();
                draft.env_file_loaded = false;
            }
            draft.scan_message = Some(format!("Picked folder `{folder_path}`"));
            cx.notify();
        }
    }

    fn scan_folder_path_action(&mut self, cx: &mut Context<Self>) {
        let (project_id, existing_id, folder_path) = match self.editor_panel.as_ref() {
            Some(EditorPanel::Folder(draft)) if !draft.folder_path.trim().is_empty() => (
                draft.project_id.clone(),
                draft.existing_id.clone(),
                draft.folder_path.trim().to_string(),
            ),
            _ => {
                self.editor_notice = Some("Choose a folder path before scanning.".to_string());
                cx.notify();
                return;
            }
        };

        if let Some(EditorPanel::Folder(draft)) = self.editor_panel.as_mut() {
            draft.is_scanning = true;
            draft.scan_message = Some(format!("Scanning `{folder_path}`..."));
        }
        cx.notify();

        let existing_labels: std::collections::BTreeSet<String> = existing_id
            .as_deref()
            .and_then(|folder_id| self.state.find_folder(&project_id, folder_id))
            .map(|lookup| {
                lookup
                    .folder
                    .commands
                    .iter()
                    .map(|command| command.label.clone())
                    .collect()
            })
            .unwrap_or_default();

        let scan_result = if self.remote_mode.is_some() {
            match self.remote_request(RemoteAction::ScanFolder {
                folder_path: folder_path.clone(),
            }) {
                Ok(result) if result.ok => match result.payload {
                    Some(RemoteActionPayload::FolderScan { scan }) => Ok(scan),
                    _ => Err("Remote host did not return a folder scan.".to_string()),
                },
                Ok(result) => Err(result
                    .message
                    .unwrap_or_else(|| "Remote folder scan failed.".to_string())),
                Err(error) => Err(format!("Remote folder scan failed: {error}")),
            }
        } else {
            scanner_service::scan_project(&folder_path)
        };
        if let Some(EditorPanel::Folder(draft)) = self.editor_panel.as_mut() {
            draft.is_scanning = false;
            let (git_branch, dependency_status) = inspect_folder_runtime_metadata(&folder_path);
            draft.git_branch = git_branch;
            draft.dependency_status = dependency_status;
            match scan_result {
                Ok(scan) => {
                    let selected_scripts: std::collections::BTreeSet<String> =
                        scanner_service::auto_selected_script_names(&scan.scripts)
                            .into_iter()
                            .filter(|name| !existing_labels.contains(name))
                            .collect();
                    let selected_port_variable =
                        scanner_service::auto_selected_port_variable(&scan.ports);
                    if draft.env_file_path.trim().is_empty() {
                        draft.env_file_path = scanner_service::default_env_file_for_dir(
                            std::path::Path::new(&folder_path),
                        )
                        .unwrap_or_default();
                    }
                    if !draft.env_file_path.trim().is_empty() {
                        draft.env_file_contents =
                            load_folder_env_contents(&folder_path, &draft.env_file_path)
                                .unwrap_or_default();
                        draft.env_file_loaded = true;
                    }
                    if let Some(variable) = selected_port_variable.clone() {
                        draft.port_variable = variable.clone();
                    }

                    let scan_message = if scan.scripts.is_empty()
                        && !scan.has_package_json
                        && !scan.has_cargo_toml
                    {
                        "No package.json or Cargo.toml was found in this folder.".to_string()
                    } else {
                        format!(
                            "Discovered {} script(s) and {} env port variable(s).",
                            scan.scripts.len(),
                            scan.ports.len()
                        )
                    };

                    draft.scan_result = Some(scan);
                    draft.selected_scanned_scripts = selected_scripts;
                    draft.selected_scanned_port_variable = selected_port_variable;
                    draft.scan_message = Some(scan_message);
                }
                Err(error) => {
                    draft.scan_result = None;
                    draft.selected_scanned_scripts.clear();
                    draft.selected_scanned_port_variable = None;
                    draft.scan_message = Some(error);
                }
            }
        }
        cx.notify();
    }

    fn load_folder_env_file_action(&mut self, cx: &mut Context<Self>) {
        let (folder_path, env_file_path) = match self.editor_panel.as_ref() {
            Some(EditorPanel::Folder(draft))
                if !draft.folder_path.trim().is_empty()
                    && !draft.env_file_path.trim().is_empty() =>
            {
                (
                    draft.folder_path.trim().to_string(),
                    draft.env_file_path.trim().to_string(),
                )
            }
            _ => {
                self.editor_notice = Some(
                    "Set both folder path and env file path before loading env contents."
                        .to_string(),
                );
                cx.notify();
                return;
            }
        };

        let env_contents = if self.remote_mode.is_some() {
            match self.remote_request(RemoteAction::ReadTextFile {
                path: std::path::Path::new(&folder_path)
                    .join(&env_file_path)
                    .to_string_lossy()
                    .to_string(),
            }) {
                Ok(result) if result.ok => match result.payload {
                    Some(RemoteActionPayload::TextFile { contents, .. }) => Some(contents),
                    _ => None,
                },
                Ok(result) => {
                    self.editor_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Remote env load failed.".to_string()),
                    );
                    cx.notify();
                    return;
                }
                Err(error) => {
                    self.editor_notice = Some(format!("Remote env load failed: {error}"));
                    cx.notify();
                    return;
                }
            }
        } else {
            load_folder_env_contents(&folder_path, &env_file_path)
        };

        match env_contents {
            Some(contents) => {
                if let Some(EditorPanel::Folder(draft)) = self.editor_panel.as_mut() {
                    draft.env_file_contents = contents;
                    draft.env_file_loaded = true;
                    draft.scan_message = Some("Loaded env file contents.".to_string());
                }
            }
            None => {
                if let Some(EditorPanel::Folder(draft)) = self.editor_panel.as_mut() {
                    draft.env_file_contents.clear();
                    draft.env_file_loaded = true;
                    draft.scan_message = Some(
                        "Env file does not exist yet. Saving the folder will create it."
                            .to_string(),
                    );
                }
            }
        }
        self.editor_active_field = Some(EditorField::Folder(FolderField::EnvContents));
        self.editor_cursor = self
            .editor_panel
            .as_ref()
            .and_then(|panel| panel.text_value(EditorField::Folder(FolderField::EnvContents)))
            .map(|value| value.chars().count())
            .unwrap_or(0);
        self.editor_needs_focus = true;
        cx.notify();
    }

    fn save_folder_env_contents(
        &mut self,
        folder_path: &str,
        env_file_path: &str,
        contents: &str,
    ) -> Result<(), String> {
        let full_path = std::path::Path::new(folder_path)
            .join(env_file_path)
            .to_string_lossy()
            .to_string();
        if self.remote_mode.is_some() {
            match self.remote_request(RemoteAction::WriteTextFile {
                path: full_path,
                contents: contents.to_string(),
            }) {
                Ok(result) if result.ok => Ok(()),
                Ok(result) => Err(result
                    .message
                    .unwrap_or_else(|| "Could not save env file.".to_string())),
                Err(error) => Err(format!("Could not save env file: {error}")),
            }
        } else {
            env_service::write_env_text(
                std::path::Path::new(folder_path)
                    .join(env_file_path)
                    .as_path(),
                contents,
            )
        }
    }

    fn open_folder_external_terminal_action(&mut self, cx: &mut Context<Self>) {
        if self.remote_mode.is_some() {
            self.editor_notice =
                Some("External terminal launch is only available on the host machine.".to_string());
            cx.notify();
            return;
        }

        let folder_path = match self.editor_panel.as_ref() {
            Some(EditorPanel::Folder(draft)) if !draft.folder_path.trim().is_empty() => {
                draft.folder_path.trim().to_string()
            }
            _ => {
                self.editor_notice =
                    Some("Set a folder path before opening a terminal.".to_string());
                cx.notify();
                return;
            }
        };
        let shell_path = external_terminal_shell_path(self.state.settings());

        match platform_service::open_terminal(&folder_path, shell_path.as_deref()) {
            Ok(()) => {
                self.editor_notice = Some("Opened external terminal.".to_string());
            }
            Err(error) => {
                self.editor_notice = Some(error);
            }
        }
        cx.notify();
    }

    fn toggle_folder_scan_script_action(&mut self, script_name: &str, cx: &mut Context<Self>) {
        if let Some(EditorPanel::Folder(draft)) = self.editor_panel.as_mut() {
            if draft.selected_scanned_scripts.contains(script_name) {
                draft.selected_scanned_scripts.remove(script_name);
            } else {
                draft
                    .selected_scanned_scripts
                    .insert(script_name.to_string());
            }
            cx.notify();
        }
    }

    fn select_folder_port_variable_action(
        &mut self,
        variable: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(EditorPanel::Folder(draft)) = self.editor_panel.as_mut() {
            draft.port_variable = variable.clone().unwrap_or_default();
            draft.selected_scanned_port_variable = variable;
            cx.notify();
        }
    }

    fn open_add_command_action(
        &mut self,
        project_id: &str,
        folder_id: &str,
        cx: &mut Context<Self>,
    ) {
        self.open_editor(
            EditorPanel::Command(CommandDraft {
                project_id: project_id.to_string(),
                folder_id: folder_id.to_string(),
                existing_id: None,
                label: String::new(),
                command: String::new(),
                args_text: String::new(),
                env_text: String::new(),
                port_text: String::new(),
                auto_restart: false,
                clear_logs_on_restart: true,
            }),
            cx,
        );
    }

    fn open_edit_command_action(&mut self, command_id: &str, cx: &mut Context<Self>) {
        if let Some(lookup) = self.state.find_command(command_id) {
            self.open_editor(
                EditorPanel::Command(CommandDraft {
                    project_id: lookup.project.id.clone(),
                    folder_id: lookup.folder.id.clone(),
                    existing_id: Some(lookup.command.id.clone()),
                    label: lookup.command.label.clone(),
                    command: lookup.command.command.clone(),
                    args_text: lookup.command.args.join(" "),
                    env_text: format_env_pairs(lookup.command.env.as_ref()),
                    port_text: lookup
                        .command
                        .port
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                    auto_restart: lookup.command.auto_restart.unwrap_or(false),
                    clear_logs_on_restart: lookup.command.clear_logs_on_restart.unwrap_or(true),
                }),
                cx,
            );
        } else {
            self.editor_notice = Some(format!("Unknown command `{command_id}`"));
            cx.notify();
        }
    }

    fn open_add_ssh_action(&mut self, cx: &mut Context<Self>) {
        self.open_editor(
            EditorPanel::Ssh(SshDraft {
                existing_id: None,
                label: String::new(),
                host: String::new(),
                port_text: "22".to_string(),
                username: String::new(),
                password: String::new(),
            }),
            cx,
        );
    }

    fn open_edit_ssh_action(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        if let Some(connection) = self.state.find_ssh_connection(connection_id).cloned() {
            self.open_editor(
                EditorPanel::Ssh(SshDraft {
                    existing_id: Some(connection.id),
                    label: connection.label,
                    host: connection.host,
                    port_text: connection.port.to_string(),
                    username: connection.username,
                    password: connection.password.unwrap_or_default(),
                }),
                cx,
            );
        } else {
            self.editor_notice = Some(format!("Unknown SSH connection `{connection_id}`"));
            cx.notify();
        }
    }

    fn apply_settings_draft(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_ref() else {
            return;
        };

        let mut settings = self.state.settings().clone();
        settings.default_terminal = draft.default_terminal.clone();
        settings.mac_terminal_profile = Some(draft.mac_terminal_profile.clone());
        settings.theme = if draft.theme.trim().is_empty() {
            "dark".to_string()
        } else {
            draft.theme.trim().to_string()
        };
        settings.log_buffer_size = match parse_optional_u32(&draft.log_buffer_size) {
            Ok(value) => match validate_log_buffer_size(value) {
                Ok(value) => value,
                Err(error) => {
                    self.editor_notice = Some(error);
                    cx.notify();
                    return;
                }
            },
            Err(error) => {
                self.editor_notice = Some(error);
                cx.notify();
                return;
            }
        };
        settings.claude_command = normalize_optional_string(&draft.claude_command);
        settings.codex_command = normalize_optional_string(&draft.codex_command);
        settings.github_token = normalize_optional_string(&draft.github_token);
        settings.notification_sound = Some(if draft.notification_sound.trim().is_empty() {
            "glass".to_string()
        } else {
            draft.notification_sound.trim().to_string()
        });
        settings.confirm_on_close = draft.confirm_on_close;
        settings.minimize_to_tray = draft.minimize_to_tray;
        settings.restore_session_on_start = Some(draft.restore_session_on_start);
        settings.terminal_font_size = match parse_optional_u16(&draft.terminal_font_size) {
            Ok(value) => match validate_terminal_font_size(value) {
                Ok(value) => value,
                Err(error) => {
                    self.editor_notice = Some(error);
                    cx.notify();
                    return;
                }
            },
            Err(error) => {
                self.editor_notice = Some(error);
                cx.notify();
                return;
            }
        };
        settings.option_as_meta = draft.option_as_meta;
        settings.copy_on_select = draft.copy_on_select;
        settings.keep_selection_on_copy = draft.keep_selection_on_copy;
        settings.show_terminal_scrollbar = draft.show_terminal_scrollbar;
        settings.shell_integration_enabled = draft.shell_integration_enabled;
        settings.terminal_mouse_override = draft.terminal_mouse_override;
        settings.terminal_read_only = draft.terminal_read_only;

        self.state.update_settings(settings);
        self.process_manager
            .set_log_buffer_size(self.state.settings().log_buffer_size as usize);
        self.save_config_state();
        self.last_dimensions = None;
        self.editor_notice = Some("Settings saved".to_string());
        cx.notify();
    }

    fn save_editor_action(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.editor_panel.clone() else {
            return;
        };

        match panel {
            EditorPanel::Settings(_) => {
                self.close_editor(cx);
            }
            EditorPanel::UiPreview(_) => {
                self.close_editor(cx);
            }
            EditorPanel::Project(draft) => {
                if draft.name.trim().is_empty() {
                    self.editor_notice = Some("Project name is required".to_string());
                    cx.notify();
                    return;
                }
                if draft.root_path.trim().is_empty() {
                    self.editor_notice = Some("Project root path is required".to_string());
                    cx.notify();
                    return;
                }

                let existing = draft
                    .existing_id
                    .as_deref()
                    .and_then(|id| self.state.find_project(id))
                    .cloned();
                let project = build_project_from_draft(&draft, existing.as_ref());
                if !self.ensure_mutation_control(cx) {
                    return;
                }
                if self.remote_mode.is_some() {
                    match self.remote_request(RemoteAction::SaveProject { project }) {
                        Ok(result) if result.ok => self.close_editor(cx),
                        Ok(result) => {
                            self.editor_notice = Some(
                                result
                                    .message
                                    .unwrap_or_else(|| "Could not save project".to_string()),
                            );
                            cx.notify();
                        }
                        Err(error) => {
                            self.editor_notice = Some(format!("Could not save project: {error}"));
                            cx.notify();
                        }
                    }
                    return;
                }
                self.state.upsert_project(project);
                self.save_config_state();
                self.save_session_state();
                self.close_editor(cx);
            }
            EditorPanel::Folder(draft) => {
                if draft.name.trim().is_empty() {
                    self.editor_notice = Some("Folder name is required".to_string());
                    cx.notify();
                    return;
                }
                if draft.folder_path.trim().is_empty() {
                    self.editor_notice = Some("Folder path is required".to_string());
                    cx.notify();
                    return;
                }

                let existing = draft
                    .existing_id
                    .as_deref()
                    .and_then(|folder_id| self.state.find_folder(&draft.project_id, folder_id))
                    .map(|lookup| lookup.folder.clone());
                if !self.ensure_mutation_control(cx) {
                    return;
                }
                if draft.env_file_loaded && !draft.env_file_path.trim().is_empty() {
                    let folder_path = draft.folder_path.trim().to_string();
                    let env_file_path = draft.env_file_path.trim().to_string();
                    let env_contents = draft.env_file_contents.clone();
                    if let Err(error) =
                        self.save_folder_env_contents(&folder_path, &env_file_path, &env_contents)
                    {
                        self.editor_notice = Some(error);
                        cx.notify();
                        return;
                    }
                }
                let folder = ProjectFolder {
                    id: draft
                        .existing_id
                        .clone()
                        .unwrap_or_else(|| next_entity_id("folder")),
                    name: draft.name.trim().to_string(),
                    folder_path: draft.folder_path.trim().to_string(),
                    commands: build_folder_commands_from_scan(&draft, existing.as_ref()),
                    env_file_path: normalize_optional_string(&draft.env_file_path),
                    port_variable: normalize_optional_string(&draft.port_variable),
                    hidden: Some(draft.hidden),
                };
                if self.remote_mode.is_some() {
                    match self.remote_request(RemoteAction::SaveFolder {
                        project_id: draft.project_id.clone(),
                        folder,
                        env_file_contents: (draft.env_file_loaded
                            && !draft.env_file_path.trim().is_empty())
                        .then_some(draft.env_file_contents.clone()),
                    }) {
                        Ok(result) if result.ok => self.close_editor(cx),
                        Ok(result) => {
                            self.editor_notice = Some(
                                result
                                    .message
                                    .unwrap_or_else(|| "Could not save folder".to_string()),
                            );
                            cx.notify();
                        }
                        Err(error) => {
                            self.editor_notice = Some(format!("Could not save folder: {error}"));
                            cx.notify();
                        }
                    }
                    return;
                }
                if !self.state.upsert_folder(&draft.project_id, folder) {
                    self.editor_notice = Some("Could not save folder".to_string());
                    cx.notify();
                    return;
                }
                self.save_config_state();
                self.save_session_state();
                self.close_editor(cx);
            }
            EditorPanel::Command(draft) => {
                if draft.label.trim().is_empty() {
                    self.editor_notice = Some("Command label is required".to_string());
                    cx.notify();
                    return;
                }
                if draft.command.trim().is_empty() {
                    self.editor_notice = Some("Command program is required".to_string());
                    cx.notify();
                    return;
                }

                let port = match parse_optional_u16(&draft.port_text) {
                    Ok(value) => value,
                    Err(error) => {
                        self.editor_notice = Some(error);
                        cx.notify();
                        return;
                    }
                };
                let command = RunCommand {
                    id: draft
                        .existing_id
                        .clone()
                        .unwrap_or_else(|| next_entity_id("command")),
                    label: draft.label.trim().to_string(),
                    command: draft.command.trim().to_string(),
                    args: parse_args_text(&draft.args_text),
                    env: parse_env_text(&draft.env_text),
                    port,
                    auto_restart: Some(draft.auto_restart),
                    clear_logs_on_restart: Some(draft.clear_logs_on_restart),
                };
                if !self.ensure_mutation_control(cx) {
                    return;
                }
                if self.remote_mode.is_some() {
                    match self.remote_request(RemoteAction::SaveCommand {
                        project_id: draft.project_id.clone(),
                        folder_id: draft.folder_id.clone(),
                        command,
                    }) {
                        Ok(result) if result.ok => self.close_editor(cx),
                        Ok(result) => {
                            self.editor_notice = Some(
                                result
                                    .message
                                    .unwrap_or_else(|| "Could not save command".to_string()),
                            );
                            cx.notify();
                        }
                        Err(error) => {
                            self.editor_notice = Some(format!("Could not save command: {error}"));
                            cx.notify();
                        }
                    }
                    return;
                }
                if !self
                    .state
                    .upsert_command(&draft.project_id, &draft.folder_id, command)
                {
                    self.editor_notice = Some("Could not save command".to_string());
                    cx.notify();
                    return;
                }
                self.save_config_state();
                self.save_session_state();
                self.close_editor(cx);
            }
            EditorPanel::Ssh(draft) => {
                if draft.label.trim().is_empty() {
                    self.editor_notice = Some("SSH label is required".to_string());
                    cx.notify();
                    return;
                }
                if draft.host.trim().is_empty() {
                    self.editor_notice = Some("SSH host is required".to_string());
                    cx.notify();
                    return;
                }
                if draft.username.trim().is_empty() {
                    self.editor_notice = Some("SSH username is required".to_string());
                    cx.notify();
                    return;
                }

                let port = match parse_optional_u16(&draft.port_text) {
                    Ok(value) => value.unwrap_or(22),
                    Err(error) => {
                        self.editor_notice = Some(error);
                        cx.notify();
                        return;
                    }
                };
                let connection = SSHConnection {
                    id: draft
                        .existing_id
                        .clone()
                        .unwrap_or_else(|| next_entity_id("ssh")),
                    label: draft.label.trim().to_string(),
                    host: draft.host.trim().to_string(),
                    port,
                    username: draft.username.trim().to_string(),
                    password: normalize_optional_string(&draft.password),
                };
                if !self.ensure_mutation_control(cx) {
                    return;
                }
                if self.remote_mode.is_some() {
                    match self.remote_request(RemoteAction::SaveSsh { connection }) {
                        Ok(result) if result.ok => self.close_editor(cx),
                        Ok(result) => {
                            self.editor_notice =
                                Some(result.message.unwrap_or_else(|| {
                                    "Could not save SSH connection".to_string()
                                }));
                            cx.notify();
                        }
                        Err(error) => {
                            self.editor_notice =
                                Some(format!("Could not save SSH connection: {error}"));
                            cx.notify();
                        }
                    }
                    return;
                }
                self.state.upsert_ssh_connection(connection);
                self.save_config_state();
                self.save_session_state();
                self.close_editor(cx);
            }
        }
    }

    fn delete_editor_action(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.editor_panel.clone() else {
            return;
        };

        match panel {
            EditorPanel::Settings(_) => {}
            EditorPanel::UiPreview(_) => {}
            EditorPanel::Project(draft) => {
                let Some(project_id) = draft.existing_id else {
                    return;
                };
                if self.remote_mode.is_some() {
                    if !self.ensure_remote_control(cx) {
                        return;
                    }
                    match self.remote_request(RemoteAction::DeleteProject {
                        project_id: project_id.clone(),
                    }) {
                        Ok(result) if result.ok => self.close_editor(cx),
                        Ok(result) => {
                            self.editor_notice = Some(
                                result
                                    .message
                                    .unwrap_or_else(|| "Could not delete project".to_string()),
                            );
                            cx.notify();
                        }
                        Err(error) => {
                            self.editor_notice = Some(format!("Could not delete project: {error}"));
                            cx.notify();
                        }
                    }
                    return;
                }
                let ai_tab_ids: Vec<String> = self
                    .state
                    .ai_tabs()
                    .filter(|tab| tab.project_id == project_id)
                    .map(|tab| tab.id.clone())
                    .collect();
                let ssh_tab_ids: Vec<String> = self
                    .state
                    .ssh_tabs()
                    .filter(|tab| tab.project_id == project_id)
                    .map(|tab| tab.id.clone())
                    .collect();
                for tab_id in ai_tab_ids {
                    let _ = self
                        .process_manager
                        .close_ai_session(&mut self.state, &tab_id);
                }
                for tab_id in ssh_tab_ids {
                    let _ = self
                        .process_manager
                        .close_ssh_session(&mut self.state, &tab_id);
                }
                self.process_manager.stop_all_for_project(&project_id);
                self.state.remove_project(&project_id);
                self.synced_session_id = None;
                self.last_dimensions = None;
                self.save_config_state();
                self.save_session_state();
                self.close_editor(cx);
            }
            EditorPanel::Folder(draft) => {
                let Some(folder_id) = draft.existing_id else {
                    return;
                };
                if self.remote_mode.is_some() {
                    if !self.ensure_remote_control(cx) {
                        return;
                    }
                    match self.remote_request(RemoteAction::DeleteFolder {
                        project_id: draft.project_id.clone(),
                        folder_id: folder_id.clone(),
                    }) {
                        Ok(result) if result.ok => self.close_editor(cx),
                        Ok(result) => {
                            self.editor_notice = Some(
                                result
                                    .message
                                    .unwrap_or_else(|| "Could not delete folder".to_string()),
                            );
                            cx.notify();
                        }
                        Err(error) => {
                            self.editor_notice = Some(format!("Could not delete folder: {error}"));
                            cx.notify();
                        }
                    }
                    return;
                }
                let command_ids: Vec<String> = self
                    .state
                    .find_folder(&draft.project_id, &folder_id)
                    .map(|lookup| {
                        lookup
                            .folder
                            .commands
                            .iter()
                            .map(|command| command.id.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                for command_id in command_ids {
                    let _ = self.process_manager.stop_server(&command_id);
                }
                self.state.remove_folder(&draft.project_id, &folder_id);
                self.synced_session_id = None;
                self.last_dimensions = None;
                self.save_config_state();
                self.save_session_state();
                self.close_editor(cx);
            }
            EditorPanel::Command(draft) => {
                let Some(command_id) = draft.existing_id else {
                    return;
                };
                if self.remote_mode.is_some() {
                    if !self.ensure_remote_control(cx) {
                        return;
                    }
                    match self.remote_request(RemoteAction::DeleteCommand {
                        project_id: draft.project_id.clone(),
                        folder_id: draft.folder_id.clone(),
                        command_id: command_id.clone(),
                    }) {
                        Ok(result) if result.ok => self.close_editor(cx),
                        Ok(result) => {
                            self.editor_notice = Some(
                                result
                                    .message
                                    .unwrap_or_else(|| "Could not delete command".to_string()),
                            );
                            cx.notify();
                        }
                        Err(error) => {
                            self.editor_notice = Some(format!("Could not delete command: {error}"));
                            cx.notify();
                        }
                    }
                    return;
                }
                let _ = self.process_manager.stop_server(&command_id);
                self.state
                    .remove_command(&draft.project_id, &draft.folder_id, &command_id);
                self.synced_session_id = None;
                self.last_dimensions = None;
                self.save_config_state();
                self.save_session_state();
                self.close_editor(cx);
            }
            EditorPanel::Ssh(draft) => {
                let Some(connection_id) = draft.existing_id else {
                    return;
                };
                if self.remote_mode.is_some() {
                    if !self.ensure_remote_control(cx) {
                        return;
                    }
                    match self.remote_request(RemoteAction::DeleteSsh {
                        connection_id: connection_id.clone(),
                    }) {
                        Ok(result) if result.ok => self.close_editor(cx),
                        Ok(result) => {
                            self.editor_notice =
                                Some(result.message.unwrap_or_else(|| {
                                    "Could not delete SSH connection".to_string()
                                }));
                            cx.notify();
                        }
                        Err(error) => {
                            self.editor_notice =
                                Some(format!("Could not delete SSH connection: {error}"));
                            cx.notify();
                        }
                    }
                    return;
                }
                let ssh_tab_ids: Vec<String> = self
                    .state
                    .ssh_tabs()
                    .filter(|tab| tab.ssh_connection_id.as_deref() == Some(connection_id.as_str()))
                    .map(|tab| tab.id.clone())
                    .collect();
                for tab_id in ssh_tab_ids {
                    let _ = self
                        .process_manager
                        .close_ssh_session(&mut self.state, &tab_id);
                }
                self.state.remove_ssh_connection(&connection_id);
                self.synced_session_id = None;
                self.last_dimensions = None;
                self.save_config_state();
                self.save_session_state();
                self.close_editor(cx);
            }
        }
    }

    fn delete_project_action(&mut self, project_id: &str, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            match self.remote_request(RemoteAction::DeleteProject {
                project_id: project_id.to_string(),
            }) {
                Ok(result) if result.ok => {
                    self.terminal_notice = None;
                }
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Could not delete project.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Could not delete project: {error}"));
                }
            }
            cx.notify();
            return;
        }

        let ai_tab_ids: Vec<String> = self
            .state
            .ai_tabs()
            .filter(|tab| tab.project_id == project_id)
            .map(|tab| tab.id.clone())
            .collect();
        let ssh_tab_ids: Vec<String> = self
            .state
            .ssh_tabs()
            .filter(|tab| tab.project_id == project_id)
            .map(|tab| tab.id.clone())
            .collect();
        for tab_id in ai_tab_ids {
            let _ = self
                .process_manager
                .close_ai_session(&mut self.state, &tab_id);
        }
        for tab_id in ssh_tab_ids {
            let _ = self
                .process_manager
                .close_ssh_session(&mut self.state, &tab_id);
        }
        self.process_manager.stop_all_for_project(project_id);
        self.state.remove_project(project_id);
        self.synced_session_id = None;
        self.last_dimensions = None;
        self.save_config_state();
        self.save_session_state();
        cx.notify();
    }

    fn delete_folder_action(&mut self, project_id: &str, folder_id: &str, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            match self.remote_request(RemoteAction::DeleteFolder {
                project_id: project_id.to_string(),
                folder_id: folder_id.to_string(),
            }) {
                Ok(result) if result.ok => {
                    self.terminal_notice = None;
                }
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Could not delete folder.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Could not delete folder: {error}"));
                }
            }
            cx.notify();
            return;
        }

        let command_ids: Vec<String> = self
            .state
            .find_folder(project_id, folder_id)
            .map(|lookup| {
                lookup
                    .folder
                    .commands
                    .iter()
                    .map(|command| command.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        for command_id in command_ids {
            let _ = self.process_manager.stop_server(&command_id);
        }
        self.state.remove_folder(project_id, folder_id);
        self.synced_session_id = None;
        self.last_dimensions = None;
        self.save_config_state();
        self.save_session_state();
        cx.notify();
    }

    fn delete_command_action(&mut self, command_id: &str, cx: &mut Context<Self>) {
        let context = self.state.find_command(command_id).map(|lookup| {
            (
                lookup.project.id.clone(),
                lookup.folder.id.clone(),
                lookup.command.id.clone(),
            )
        });
        let Some((project_id, folder_id, command_id)) = context else {
            self.editor_notice = Some(format!("Unknown command `{command_id}`"));
            cx.notify();
            return;
        };

        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            match self.remote_request(RemoteAction::DeleteCommand {
                project_id,
                folder_id,
                command_id: command_id.clone(),
            }) {
                Ok(result) if result.ok => {
                    self.terminal_notice = None;
                }
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Could not delete command.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Could not delete command: {error}"));
                }
            }
            cx.notify();
            return;
        }

        let _ = self.process_manager.stop_server(&command_id);
        self.state
            .remove_command(&project_id, &folder_id, &command_id);
        self.synced_session_id = None;
        self.last_dimensions = None;
        self.save_config_state();
        self.save_session_state();
        cx.notify();
    }

    fn delete_ssh_action(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            match self.remote_request(RemoteAction::DeleteSsh {
                connection_id: connection_id.to_string(),
            }) {
                Ok(result) if result.ok => {
                    self.terminal_notice = None;
                }
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Could not delete SSH connection.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice =
                        Some(format!("Could not delete SSH connection: {error}"));
                }
            }
            cx.notify();
            return;
        }

        let ssh_tab_ids: Vec<String> = self
            .state
            .ssh_tabs()
            .filter(|tab| tab.ssh_connection_id.as_deref() == Some(connection_id))
            .map(|tab| tab.id.clone())
            .collect();
        for tab_id in ssh_tab_ids {
            let _ = self
                .process_manager
                .close_ssh_session(&mut self.state, &tab_id);
        }
        self.state.remove_ssh_connection(connection_id);
        self.synced_session_id = None;
        self.last_dimensions = None;
        self.save_config_state();
        self.save_session_state();
        cx.notify();
    }

    fn focus_editor_field(
        &mut self,
        field: EditorField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cursor = self
            .editor_panel
            .as_ref()
            .and_then(|panel| panel.text_value(field))
            .map(|value| value.chars().count())
            .unwrap_or(0);
        self.focus_editor_field_at(field, cursor, false, window, cx);
    }

    fn focus_editor_field_at(
        &mut self,
        field: EditorField,
        cursor: usize,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
            draft.open_picker = None;
        }
        let clamped_cursor = self
            .editor_panel
            .as_ref()
            .and_then(|panel| panel.text_value(field))
            .map(|value| cursor.min(value.chars().count()))
            .unwrap_or(cursor);
        if shift && self.editor_active_field == Some(field) {
            // Extend selection: anchor stays (or is set to current cursor)
            if self.editor_selection_anchor.is_none() {
                self.editor_selection_anchor = Some(self.editor_cursor);
            }
        } else {
            self.editor_selection_anchor = None;
        }
        self.editor_active_field = Some(field);
        self.editor_cursor = clamped_cursor;
        self.is_selecting_editor = true;
        self.focus_editor(window);
        cx.notify();
    }

    fn drag_editor_field_to(
        &mut self,
        field: EditorField,
        cursor: usize,
        cx: &mut Context<Self>,
    ) {
        if !self.is_selecting_editor || self.editor_active_field != Some(field) {
            return;
        }
        let clamped_cursor = self
            .editor_panel
            .as_ref()
            .and_then(|panel| panel.text_value(field))
            .map(|value| cursor.min(value.chars().count()))
            .unwrap_or(cursor);
        if self.editor_selection_anchor.is_none() {
            self.editor_selection_anchor = Some(self.editor_cursor);
        }
        self.editor_cursor = clamped_cursor;
        cx.notify();
    }

    fn apply_editor_action(
        &mut self,
        action: EditorAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            EditorAction::FocusField(field) => self.focus_editor_field(field, window, cx),
            EditorAction::Save => self.save_editor_action(cx),
            EditorAction::Delete => self.delete_editor_action(cx),
            EditorAction::Close => self.close_editor(cx),
            EditorAction::PickFolderPath => self.pick_folder_path_action(cx),
            EditorAction::ScanFolderPath => self.scan_folder_path_action(cx),
            EditorAction::ToggleFolderScanScript(script_name) => {
                self.toggle_folder_scan_script_action(&script_name, cx)
            }
            EditorAction::SelectFolderPortVariable(variable) => {
                self.select_folder_port_variable_action(variable, cx)
            }
            EditorAction::LoadFolderEnvFile => self.load_folder_env_file_action(cx),
            EditorAction::OpenFolderExternalTerminal => {
                self.open_folder_external_terminal_action(cx)
            }
            EditorAction::ExportConfig => self.export_config_action(cx),
            EditorAction::ImportConfigMerge => {
                self.import_config_action(ConfigImportMode::Merge, cx)
            }
            EditorAction::ImportConfigReplace => {
                self.import_config_action(ConfigImportMode::Replace, cx)
            }
            EditorAction::CheckForUpdates => self.check_for_updates_action(cx),
            EditorAction::DownloadUpdate => self.download_update_action(cx),
            EditorAction::InstallUpdate => self.install_update_action(cx),
            EditorAction::OpenUiPreview => self.open_ui_preview_action(cx),
            EditorAction::CycleDefaultTerminal => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.default_terminal =
                        workspace::next_default_terminal(draft.default_terminal.clone());
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::CycleMacTerminalProfile => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.mac_terminal_profile =
                        workspace::next_mac_terminal_profile(draft.mac_terminal_profile.clone());
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::CycleNotificationSound => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.notification_sound =
                        workspace::next_notification_sound(&draft.notification_sound);
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::PreviewNotificationSound => self.preview_notification_sound_action(cx),
            EditorAction::ToggleSettingsPicker(picker) => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.open_picker = if draft.open_picker == Some(picker) {
                        None
                    } else {
                        Some(picker)
                    };
                    self.editor_active_field = None;
                    cx.notify();
                }
            }
            EditorAction::SelectDefaultTerminal(terminal) => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.default_terminal = terminal;
                    draft.open_picker = None;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::SelectMacTerminalProfile(profile) => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.mac_terminal_profile = profile;
                    draft.open_picker = None;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::SelectNotificationSound(sound_id) => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.notification_sound = sound_id;
                    draft.open_picker = None;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::SetTerminalFontSize(size) => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.terminal_font_size = size.to_string();
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::ToggleConfirmOnClose => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.confirm_on_close = !draft.confirm_on_close;
                    draft.open_picker = None;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::ToggleMinimizeToTray => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.minimize_to_tray = !draft.minimize_to_tray;
                    draft.open_picker = None;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::ToggleRestoreSession => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.restore_session_on_start = !draft.restore_session_on_start;
                    draft.open_picker = None;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::ToggleOptionAsMeta => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.option_as_meta = !draft.option_as_meta;
                    draft.open_picker = None;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::ToggleCopyOnSelect => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.copy_on_select = !draft.copy_on_select;
                    draft.open_picker = None;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::ToggleKeepSelectionOnCopy => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.keep_selection_on_copy = !draft.keep_selection_on_copy;
                    draft.open_picker = None;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::ToggleShowTerminalScrollbar => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.show_terminal_scrollbar = !draft.show_terminal_scrollbar;
                    draft.open_picker = None;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::ToggleShellIntegrationEnabled => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.shell_integration_enabled = !draft.shell_integration_enabled;
                    draft.open_picker = None;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::ToggleTerminalMouseOverride => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.terminal_mouse_override = !draft.terminal_mouse_override;
                    draft.open_picker = None;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::ToggleTerminalReadOnly => {
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.terminal_read_only = !draft.terminal_read_only;
                    draft.open_picker = None;
                    self.apply_settings_draft(cx);
                }
            }
            EditorAction::ToggleRemoteHosting => {
                if !self.ensure_mutation_control(cx) {
                    return;
                }
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.remote_host_enabled = !draft.remote_host_enabled;
                    self.remote_machine_state.host.enabled = draft.remote_host_enabled;
                    self.remote_machine_state.host.bind_address =
                        draft.remote_bind_address.trim().to_string();
                    self.remote_machine_state.host.port = parse_optional_u16(&draft.remote_port)
                        .ok()
                        .flatten()
                        .unwrap_or(43871);
                    self.remote_machine_state.host.pairing_token =
                        draft.remote_pairing_token.clone();
                    self.remote_host_service
                        .apply_config(self.remote_machine_state.host.clone());
                    self.persist_remote_machine_state();
                    self.sync_settings_remote_draft();
                    cx.notify();
                }
            }
            EditorAction::ToggleRemoteKeepHostingInBackground => {
                if !self.ensure_mutation_control(cx) {
                    return;
                }
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.remote_keep_hosting_in_background =
                        !draft.remote_keep_hosting_in_background;
                    self.remote_machine_state.host.keep_hosting_in_background =
                        draft.remote_keep_hosting_in_background;
                    self.remote_host_service
                        .apply_config(self.remote_machine_state.host.clone());
                    self.persist_remote_machine_state();
                    self.sync_settings_remote_draft();
                    cx.notify();
                }
            }
            EditorAction::RegenerateRemotePairingToken => {
                if !self.ensure_mutation_control(cx) {
                    return;
                }
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    let token = remote::generate_pairing_token();
                    draft.remote_pairing_token = token.clone();
                    self.remote_machine_state.host.pairing_token = token;
                    self.remote_host_service
                        .apply_config(self.remote_machine_state.host.clone());
                    self.persist_remote_machine_state();
                    self.sync_settings_remote_draft();
                    cx.notify();
                }
            }
            EditorAction::CopyRemotePairingToken => {
                self.copy_remote_pairing_token_action(cx);
            }
            EditorAction::ConnectRemoteHost => {
                let Some((
                    remote_connect_in_flight,
                    remote_connect_address,
                    remote_connect_port,
                    remote_connect_token,
                )) = self.editor_panel.as_ref().and_then(|panel| match panel {
                    EditorPanel::Settings(draft) => Some((
                        draft.remote_connect_in_flight,
                        draft.remote_connect_address.clone(),
                        draft.remote_connect_port.clone(),
                        draft.remote_connect_token.clone(),
                    )),
                    _ => None,
                })
                else {
                    return;
                };
                if remote_connect_in_flight {
                    return;
                }
                let port = parse_optional_u16(&remote_connect_port)
                    .map_err(|error| {
                        self.editor_notice = Some(error);
                        if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                            draft.remote_connect_status = self.editor_notice.clone();
                            draft.remote_connect_status_is_error = true;
                            draft.remote_connect_in_flight = false;
                        }
                        cx.notify();
                    })
                    .ok()
                    .flatten()
                    .unwrap_or(43871);
                self.editor_notice = None;
                match self.begin_connect_remote_host(
                    remote_connect_address.trim().to_string(),
                    port,
                    normalize_optional_string(&remote_connect_token),
                    cx,
                ) {
                    Ok(()) => {}
                    Err(error) => {
                        self.editor_notice = Some(error);
                        if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                            draft.remote_connect_in_flight = false;
                            draft.remote_connect_status = self.editor_notice.clone();
                            draft.remote_connect_status_is_error = true;
                        }
                    }
                }
                cx.notify();
            }
            EditorAction::DisconnectRemoteHost => {
                self.disconnect_remote_host(Some("Disconnected from remote host.".to_string()));
                cx.notify();
            }
            EditorAction::TakeRemoteControl => {
                if let Some(remote_mode) = self.remote_mode.as_ref() {
                    remote_mode.client.take_control();
                }
                self.editor_notice = Some("This client now controls the remote host.".to_string());
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.remote_connect_status = self.editor_notice.clone();
                    draft.remote_connect_status_is_error = false;
                }
                self.sync_settings_remote_draft();
                cx.notify();
            }
            EditorAction::ReleaseRemoteControl => {
                if let Some(remote_mode) = self.remote_mode.as_ref() {
                    remote_mode.client.release_control();
                }
                self.editor_notice =
                    Some("This client released control and is now a viewer.".to_string());
                if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                    draft.remote_connect_status = self.editor_notice.clone();
                    draft.remote_connect_status_is_error = false;
                }
                self.sync_settings_remote_draft();
                cx.notify();
            }
            EditorAction::TakeHostControl => {
                self.remote_host_service.take_local_control();
                self.editor_notice = Some("This machine controls the host again.".to_string());
                self.sync_settings_remote_draft();
                cx.notify();
            }
            EditorAction::UseKnownRemoteHost(server_id) => {
                if let Some(host) = self
                    .remote_machine_state
                    .known_hosts
                    .iter()
                    .find(|host| host.server_id == server_id)
                    .cloned()
                {
                    if let Some(EditorPanel::Settings(draft)) = self.editor_panel.as_mut() {
                        draft.remote_connect_address = host.address.clone();
                        draft.remote_connect_port = host.port.to_string();
                        draft.remote_connect_token.clear();
                        draft.open_picker = None;
                    }
                    self.connect_known_remote_host(host, cx);
                }
                cx.notify();
            }
            EditorAction::ForgetKnownRemoteHost(server_id) => {
                self.remote_machine_state
                    .known_hosts
                    .retain(|host| host.server_id != server_id);
                self.persist_remote_machine_state();
                self.sync_settings_remote_draft();
                self.editor_notice = Some("Removed saved remote host.".to_string());
                cx.notify();
            }
            EditorAction::RevokeRemoteClient(client_id) => {
                self.remote_machine_state
                    .host
                    .paired_clients
                    .retain(|client| client.client_id != client_id);
                self.remote_host_service.revoke_paired_client(&client_id);
                self.remote_host_service
                    .apply_config(self.remote_machine_state.host.clone());
                self.persist_remote_machine_state();
                self.sync_settings_remote_draft();
                self.editor_notice = Some("Revoked paired remote client.".to_string());
                cx.notify();
            }
            EditorAction::ToggleProjectPinned => {
                if let Some(EditorPanel::Project(draft)) = self.editor_panel.as_mut() {
                    draft.pinned = !draft.pinned;
                    cx.notify();
                }
            }
            EditorAction::ToggleProjectSaveLogs => {
                if let Some(EditorPanel::Project(draft)) = self.editor_panel.as_mut() {
                    draft.save_log_files = !draft.save_log_files;
                    cx.notify();
                }
            }
            EditorAction::ToggleFolderHidden => {
                if let Some(EditorPanel::Folder(draft)) = self.editor_panel.as_mut() {
                    draft.hidden = !draft.hidden;
                    cx.notify();
                }
            }
            EditorAction::ToggleCommandAutoRestart => {
                if let Some(EditorPanel::Command(draft)) = self.editor_panel.as_mut() {
                    draft.auto_restart = !draft.auto_restart;
                    cx.notify();
                }
            }
            EditorAction::ToggleCommandClearLogs => {
                if let Some(EditorPanel::Command(draft)) = self.editor_panel.as_mut() {
                    draft.clear_logs_on_restart = !draft.clear_logs_on_restart;
                    cx.notify();
                }
            }
        }
    }

    fn handle_editor_mouse_down(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        if self.add_project_wizard.is_some() {
            return;
        }
        self.focus_editor(window);
    }

    fn handle_editor_mouse_up(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
        self.is_selecting_editor = false;
    }

    fn handle_wizard_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(wizard) = self.add_project_wizard.as_mut() else {
            return false;
        };
        let step = wizard.step;

        let key = event.keystroke.key.to_ascii_lowercase();
        let modifiers = event.keystroke.modifiers;
        let secondary = modifiers.control || modifiers.platform;

        if key == "escape" {
            if step == 2 {
                wizard.step = 1;
                cx.notify();
            } else {
                self.add_project_wizard = None;
                cx.notify();
            }
            window.prevent_default();
            return true;
        }
        if key == "enter" {
            if step == 2 {
                self.wizard_create_action(cx);
            }
            // Step 1: don't auto-configure on Enter (user should click Configure)
            window.prevent_default();
            return true;
        }

        // Only step 1 has a text input
        if step != 1 {
            return true;
        }

        let paste_text = if secondary && key == "v" {
            cx.read_from_clipboard().and_then(|item| item.text())
        } else {
            None
        };

        let changed = apply_text_key_to_string(
            &mut wizard.name,
            &mut wizard.cursor,
            &mut None,
            event,
            paste_text.as_deref(),
            false,
            false,
        );
        if changed {
            wizard.name_focused = true;
            cx.notify();
            window.prevent_default();
        }
        true
    }

    fn handle_editor_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.handle_wizard_key(event, window, cx) {
            return;
        }
        if self.editor_panel.is_none() {
            return;
        }

        let key = event.keystroke.key.to_ascii_lowercase();
        let modifiers = event.keystroke.modifiers;
        let secondary = modifiers.control || modifiers.platform;

        if secondary && key == "s" {
            self.save_editor_action(cx);
            window.prevent_default();
            return;
        }
        if key == "escape" {
            self.close_editor(cx);
            window.prevent_default();
            return;
        }

        let Some(field) = self.editor_active_field else {
            return;
        };

        // Copy selected text
        if secondary && key == "c" {
            if let Some(anchor) = self.editor_selection_anchor {
                if let Some(panel) = self.editor_panel.as_ref() {
                    if let Some(value) = panel.text_value(field) {
                        let (start, end) = if anchor < self.editor_cursor {
                            (anchor, self.editor_cursor)
                        } else {
                            (self.editor_cursor, anchor)
                        };
                        if start != end {
                            let selected: String =
                                value.chars().skip(start).take(end - start).collect();
                            cx.write_to_clipboard(ClipboardItem::new_string(selected));
                        }
                    }
                }
            }
            window.prevent_default();
            return;
        }

        // Cut selected text
        if secondary && key == "x" {
            if let Some(anchor) = self.editor_selection_anchor {
                if let Some(panel) = self.editor_panel.as_ref() {
                    if let Some(value) = panel.text_value(field) {
                        let (start, end) = if anchor < self.editor_cursor {
                            (anchor, self.editor_cursor)
                        } else {
                            (self.editor_cursor, anchor)
                        };
                        if start != end {
                            let selected: String =
                                value.chars().skip(start).take(end - start).collect();
                            cx.write_to_clipboard(ClipboardItem::new_string(selected));
                        }
                    }
                }
            }
            if let Some(panel) = self.editor_panel.as_mut() {
                if let Some(value) = panel.text_value_mut(field) {
                    let mut chars: Vec<char> = value.chars().collect();
                    delete_selection(&mut chars, &mut self.editor_cursor, &mut self.editor_selection_anchor);
                    *value = chars.into_iter().collect();
                }
            }
            self.editor_notice = None;
            cx.notify();
            window.prevent_default();
            return;
        }

        let paste_text = if secondary && key == "v" {
            cx.read_from_clipboard().and_then(|item| item.text())
        } else {
            None
        };
        let mut changed = false;
        if let Some(panel) = self.editor_panel.as_mut() {
            if let Some(value) = panel.text_value_mut(field) {
                changed = apply_text_key_to_string(
                    value,
                    &mut self.editor_cursor,
                    &mut self.editor_selection_anchor,
                    event,
                    paste_text.as_deref(),
                    field.is_numeric(),
                    field.allows_newlines(),
                );
            }
        }

        if changed {
            if matches!(self.editor_panel, Some(EditorPanel::Settings(_))) {
                self.apply_settings_draft(cx);
            } else {
                self.editor_notice = None;
                cx.notify();
            }
            window.prevent_default();
        }
    }

    fn sync_terminal_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> view::TerminalPaneModel {
        if self.remote_mode.is_some() {
            let active_spec = self.state.active_terminal_spec();
            let active_tab = self.state.active_tab().cloned();
            let active_tab_type = active_tab.as_ref().map(|tab| tab.tab_type.clone());
            let active_session = self.current_active_session_view();
            let reconnecting = self
                .remote_mode
                .as_ref()
                .and_then(|remote_mode| remote_mode.reconnect.as_ref())
                .is_some();

            if active_tab_type.is_some() {
                self.splash_image = None;
                if self.synced_session_id.as_deref() != Some(active_spec.session_id.as_str()) {
                    self.synced_session_id = Some(active_spec.session_id.clone());
                    self.last_dimensions = None;
                    self.active_port_state = None;
                }
                if let Some(remote_mode) = self.remote_mode.as_mut() {
                    let next_focused = Some(active_spec.session_id.clone());
                    remote_mode.client.set_focused_session(next_focused);
                }
                let dimensions = self.terminal_dimensions(window);
                if terminal_view_needs_resize(
                    self.last_dimensions,
                    active_session.as_ref(),
                    dimensions,
                ) {
                    if self.remote_has_control() {
                        if let Some(remote_mode) = self.remote_mode.as_ref() {
                            remote_mode
                                .client
                                .apply_local_terminal_resize(&active_spec.session_id, dimensions);
                            remote_mode
                                .client
                                .send_terminal_resize(active_spec.session_id.clone(), dimensions);
                        }
                        self.last_dimensions = Some(dimensions);
                    }
                }
            } else if !self.state.open_tabs.is_empty() {
                self.ensure_splash_image(cx);
            }

            self.terminal_notice = if reconnecting {
                Some("Reconnecting to remote host...".to_string())
            } else {
                match active_tab_type {
                    Some(TabType::Server) if active_session.is_none() => {
                        Some("Remote server session is not available yet.".to_string())
                    }
                    Some(TabType::Ssh) if active_session.is_none() => {
                        Some("Remote SSH session is disconnected.".to_string())
                    }
                    Some(TabType::Claude) | Some(TabType::Codex) if active_session.is_none() => {
                        Some("Remote AI session is not available yet.".to_string())
                    }
                    Some(_) => None,
                    None => self.terminal_notice.clone(),
                }
            };

            if active_tab_type == Some(TabType::Ssh) && self.terminal_input_block_reason().is_none()
            {
                self.maybe_auto_submit_ssh_password(active_session.as_ref());
            } else {
                self.ssh_password_prompt_state = None;
            }

            if !self.did_focus_terminal {
                window.focus(&self.terminal_focus);
                self.did_focus_terminal = true;
            }

            self.sync_terminal_focus(Some(active_spec.session_id.clone()));

            let selection = active_session
                .as_ref()
                .and_then(|session| self.selection_snapshot(&session.screen));
            let runtime_controls = self.runtime_controls_model(
                active_tab_type.clone(),
                &active_spec,
                active_session.as_ref(),
            );
            let terminal_metrics = self.terminal_render_metrics(window);
            let blocking_notice = self.terminal_input_block_reason().or_else(|| {
                active_session.as_ref().and_then(|session| {
                    session
                        .runtime
                        .awaiting_external_editor
                        .then_some("Save and close text editor to continue...".to_string())
                })
            });

            let search_highlight = self.current_search_highlight(active_session.as_ref());
            let scrollbar = self.terminal_scrollbar_model(active_session.as_ref());
            let has_active_tab = active_tab_type.is_some();
            return view::TerminalPaneModel {
                active_project: if has_active_tab {
                    self.state
                        .active_project()
                        .map(|project| project.name.clone())
                        .unwrap_or_else(|| "No project selected".to_string())
                } else {
                    String::new()
                },
                session_label: if has_active_tab {
                    active_spec.display_label
                } else {
                    String::new()
                },
                active_tab_type,
                session: active_session,
                startup_notice: self
                    .startup_notice
                    .clone()
                    .or_else(|| self.terminal_notice.clone()),
                blocking_notice,
                debug_enabled: self.process_manager.debug_enabled(),
                font_size: self.terminal_font_size(),
                cell_width: terminal_metrics.cell_width,
                line_height: terminal_metrics.line_height,
                selection,
                search: self.terminal_search_model(),
                search_highlight,
                scrollbar,
                runtime_controls,
                splash_image: self.splash_image.clone(),
            };
        }

        let mut active_spec = self.state.active_terminal_spec();
        let active_tab = self.state.active_tab().cloned();
        let active_tab_type = active_tab.as_ref().map(|tab| tab.tab_type.clone());
        let mut active_session = None;
        let local_has_resize_control = self.local_host_has_control();

        if active_tab_type.is_some() {
            self.splash_image = None;
        }

        match active_tab_type {
            Some(TabType::Server) => {
                self.process_manager
                    .set_active_session(active_spec.session_id.clone());
                if self.synced_session_id.as_deref() != Some(active_spec.session_id.as_str()) {
                    self.synced_session_id = Some(active_spec.session_id.clone());
                    self.last_dimensions = None;
                    self.active_port_state = None;
                }

                let server_runtime = self
                    .process_manager
                    .runtime_state()
                    .sessions
                    .get(&active_spec.session_id)
                    .cloned();
                let session_live = server_runtime
                    .as_ref()
                    .map(|session| session.status.is_live())
                    .unwrap_or(false);
                let interactive_prompt = server_runtime
                    .as_ref()
                    .map(|session| session.interactive_shell)
                    .unwrap_or(false);
                if session_live || interactive_prompt {
                    let dimensions = self.terminal_dimensions(window);
                    let current_view = if local_has_resize_control {
                        self.process_manager.session_view(&active_spec.session_id)
                    } else {
                        self.local_viewer_session_view(&active_spec.session_id, dimensions)
                    };
                    if local_has_resize_control
                        && terminal_view_needs_resize(
                            self.last_dimensions,
                            current_view.as_ref(),
                            dimensions,
                        )
                        && self
                            .process_manager
                            .resize_session(&active_spec.session_id, dimensions)
                            .is_ok()
                    {
                        self.last_dimensions = Some(dimensions);
                    }
                    self.terminal_notice = None;
                    active_session = if local_has_resize_control {
                        self.process_manager.session_view(&active_spec.session_id)
                    } else {
                        self.local_viewer_session_view(&active_spec.session_id, dimensions)
                    };
                } else if self.terminal_notice.is_none() {
                    self.terminal_notice = Some(
                        "Server session is not running. Start it from the sidebar.".to_string(),
                    );
                }
            }
            Some(TabType::Claude) | Some(TabType::Codex) => {
                let dimensions = self.terminal_dimensions(window);
                if let Some(active_tab) = active_tab.as_ref() {
                    let session_live = active_tab
                        .pty_session_id
                        .as_deref()
                        .and_then(|session_id| {
                            self.process_manager
                                .runtime_state()
                                .sessions
                                .get(session_id)
                                .cloned()
                        })
                        .map(|session| session.status.is_live())
                        .unwrap_or(false);

                    if !session_live {
                        match self.process_manager.ensure_ai_session_for_tab(
                            &mut self.state,
                            &active_tab.id,
                            dimensions,
                            true,
                            false,
                        ) {
                            Ok(_) => {
                                self.save_session_state();
                                active_spec = self.state.active_terminal_spec();
                                self.terminal_notice = None;
                            }
                            Err(error) => {
                                self.terminal_notice =
                                    Some(format!("Failed to restore AI session: {error}"));
                            }
                        }
                    }
                }

                self.process_manager
                    .set_active_session(active_spec.session_id.clone());
                if self.synced_session_id.as_deref() != Some(active_spec.session_id.as_str()) {
                    self.synced_session_id = Some(active_spec.session_id.clone());
                    self.last_dimensions = None;
                }

                let current_view = if local_has_resize_control {
                    self.process_manager.active_session()
                } else {
                    self.local_viewer_session_view(&active_spec.session_id, dimensions)
                };
                if local_has_resize_control
                    && terminal_view_needs_resize(
                        self.last_dimensions,
                        current_view.as_ref(),
                        dimensions,
                    )
                    && self
                        .process_manager
                        .resize_session(&active_spec.session_id, dimensions)
                        .is_ok()
                {
                    self.last_dimensions = Some(dimensions);
                }
                active_session = if local_has_resize_control {
                    self.process_manager.active_session()
                } else {
                    self.local_viewer_session_view(&active_spec.session_id, dimensions)
                };
            }
            Some(TabType::Ssh) => {
                if let Some(active_tab) = active_tab.as_ref() {
                    let session_live = active_tab
                        .pty_session_id
                        .as_deref()
                        .and_then(|session_id| {
                            self.process_manager
                                .runtime_state()
                                .sessions
                                .get(session_id)
                                .cloned()
                        })
                        .map(|session| {
                            session.status.is_live()
                                && matches!(session.session_kind, crate::state::SessionKind::Ssh)
                        })
                        .unwrap_or(false);

                    if session_live {
                        active_spec = self.state.active_terminal_spec();
                        self.process_manager
                            .set_active_session(active_spec.session_id.clone());
                        if self.synced_session_id.as_deref()
                            != Some(active_spec.session_id.as_str())
                        {
                            self.synced_session_id = Some(active_spec.session_id.clone());
                            self.last_dimensions = None;
                        }

                        let dimensions = self.terminal_dimensions(window);
                        let current_view = if local_has_resize_control {
                            self.process_manager.session_view(&active_spec.session_id)
                        } else {
                            self.local_viewer_session_view(&active_spec.session_id, dimensions)
                        };
                        if local_has_resize_control
                            && terminal_view_needs_resize(
                                self.last_dimensions,
                                current_view.as_ref(),
                                dimensions,
                            )
                            && self
                                .process_manager
                                .resize_session(&active_spec.session_id, dimensions)
                                .is_ok()
                        {
                            self.last_dimensions = Some(dimensions);
                        }

                        self.terminal_notice = None;
                        active_session = if local_has_resize_control {
                            self.process_manager.session_view(&active_spec.session_id)
                        } else {
                            self.local_viewer_session_view(&active_spec.session_id, dimensions)
                        };
                    } else {
                        if self.synced_session_id.as_deref() != Some(active_tab.id.as_str()) {
                            self.synced_session_id = Some(active_tab.id.clone());
                            self.last_dimensions = None;
                        }
                        self.terminal_notice = Some(
                            "SSH session is disconnected. Connect from the sidebar.".to_string(),
                        );
                    }
                }
            }
            _ if !self.state.open_tabs.is_empty() => {
                // Tabs exist but none is selected — show splash image.
                self.ensure_splash_image(cx);
            }
            _ => {
                self.process_manager
                    .set_active_session(active_spec.session_id.clone());
                if self.synced_session_id.as_deref() != Some(active_spec.session_id.as_str()) {
                    if let Err(error) = self.process_manager.spawn_shell_session(
                        active_spec.session_id.clone(),
                        &active_spec.cwd,
                        SessionDimensions::default(),
                        Some(self.state.settings().default_terminal.clone()),
                        self.state.settings().mac_terminal_profile.clone(),
                    ) {
                        self.terminal_notice =
                            Some(format!("Failed to start shell session: {error}"));
                    } else {
                        self.terminal_notice = None;
                    }
                    self.synced_session_id = Some(active_spec.session_id.clone());
                    self.last_dimensions = None;
                }

                let dimensions = self.terminal_dimensions(window);
                let current_view = if local_has_resize_control {
                    self.process_manager.active_session()
                } else {
                    self.local_viewer_session_view(&active_spec.session_id, dimensions)
                };
                if local_has_resize_control
                    && terminal_view_needs_resize(
                        self.last_dimensions,
                        current_view.as_ref(),
                        dimensions,
                    )
                    && self
                        .process_manager
                        .resize_session(&active_spec.session_id, dimensions)
                        .is_ok()
                {
                    self.last_dimensions = Some(dimensions);
                }
                active_session = if local_has_resize_control {
                    self.process_manager.active_session()
                } else {
                    self.local_viewer_session_view(&active_spec.session_id, dimensions)
                };
            }
        }

        if active_tab_type == Some(TabType::Ssh) && self.terminal_input_block_reason().is_none() {
            self.maybe_auto_submit_ssh_password(active_session.as_ref());
        } else {
            self.ssh_password_prompt_state = None;
        }

        if !self.did_focus_terminal {
            window.focus(&self.terminal_focus);
            self.did_focus_terminal = true;
        }

        self.sync_terminal_focus(Some(active_spec.session_id.clone()));

        let selection = active_session
            .as_ref()
            .and_then(|session| self.selection_snapshot(&session.screen));
        let runtime_controls = self.runtime_controls_model(
            active_tab_type.clone(),
            &active_spec,
            active_session.as_ref(),
        );
        let terminal_metrics = self.terminal_render_metrics(window);
        let blocking_notice = self.terminal_input_block_reason().or_else(|| {
            active_session.as_ref().and_then(|session| {
                session
                    .runtime
                    .awaiting_external_editor
                    .then_some("Save and close text editor to continue...".to_string())
            })
        });

        let search_highlight = self.current_search_highlight(active_session.as_ref());
        let scrollbar = self.terminal_scrollbar_model(active_session.as_ref());
        let has_active_tab = active_tab_type.is_some();
        view::TerminalPaneModel {
            active_project: if has_active_tab {
                self.state
                    .active_project()
                    .map(|project| project.name.clone())
                    .unwrap_or_else(|| "No project selected".to_string())
            } else {
                String::new()
            },
            session_label: if has_active_tab {
                active_spec.display_label
            } else {
                String::new()
            },
            active_tab_type,
            session: active_session,
            startup_notice: self
                .startup_notice
                .clone()
                .or_else(|| self.terminal_notice.clone()),
            blocking_notice,
            debug_enabled: self.process_manager.debug_enabled(),
            font_size: self.terminal_font_size(),
            cell_width: terminal_metrics.cell_width,
            line_height: terminal_metrics.line_height,
            selection,
            search: self.terminal_search_model(),
            search_highlight,
            scrollbar,
            runtime_controls,
            splash_image: self.splash_image.clone(),
        }
    }

    fn focus_terminal(&mut self, window: &mut Window) {
        let session_id = if self.remote_mode.is_some() {
            self.active_remote_terminal_session_id()
                .or_else(|| Some(self.state.active_terminal_spec().session_id))
        } else {
            Some(self.state.active_terminal_spec().session_id)
        };
        self.sync_terminal_focus(session_id);
        window.focus(&self.terminal_focus);
        self.did_focus_terminal = true;
    }

    fn sync_terminal_focus(&mut self, next_session_id: Option<String>) {
        if self.focused_terminal_session_id == next_session_id {
            return;
        }

        if let Some(previous_session_id) = self.focused_terminal_session_id.take() {
            let _ = self
                .process_manager
                .report_focus(&previous_session_id, false);
        }

        if let Some(session_id) = next_session_id.clone() {
            let _ = self.process_manager.report_focus(&session_id, true);
        }

        self.focused_terminal_session_id = next_session_id;
    }

    fn terminal_scrollbar_model(
        &self,
        session: Option<&crate::terminal::session::TerminalSessionView>,
    ) -> Option<view::TerminalScrollbarModel> {
        let session = session?;
        if !self.terminal_has_scrollbar(session) {
            return None;
        }

        let total_lines = session.screen.total_lines.max(session.screen.rows.max(1));
        let visible_lines = session.screen.rows.max(1);
        if total_lines <= visible_lines {
            return None;
        }

        let max_offset = session.screen.history_size.max(1);
        let thumb_height_ratio = visible_lines as f32 / total_lines as f32;
        let thumb_top_ratio = self
            .terminal_scrollbar_drag
            .map(|drag| drag.thumb_top_ratio)
            .unwrap_or_else(|| {
                scrollbar_thumb_top_ratio(session.screen.display_offset, max_offset)
            });

        Some(view::TerminalScrollbarModel {
            thumb_top_ratio: thumb_top_ratio.clamp(0.0, 1.0),
            thumb_height_ratio,
        })
    }

    fn terminal_has_scrollbar(
        &self,
        session: &crate::terminal::session::TerminalSessionView,
    ) -> bool {
        self.state.settings().show_terminal_scrollbar
            && session.screen.total_lines > session.screen.rows.max(1)
    }

    fn terminal_scrollbar_geometry(
        &self,
        window: &Window,
        session: &crate::terminal::session::TerminalSessionView,
    ) -> Option<TerminalScrollbarGeometry> {
        if !self.terminal_has_scrollbar(session) {
            return None;
        }

        let layout = self.terminal_viewport_layout(window, session.runtime.exit.is_some())?;
        let total_lines = session.screen.total_lines.max(session.screen.rows.max(1));
        let visible_lines = session.screen.rows.max(1);
        let max_offset = session.screen.history_size.max(1);

        let left = layout.left + layout.available_width - view::TERMINAL_SCROLLBAR_WIDTH_PX;
        let top = layout.top - 2.0;
        let width = view::TERMINAL_SCROLLBAR_WIDTH_PX;
        let height = layout.available_height + 4.0;
        let track_top = top + view::TERMINAL_SCROLLBAR_TRACK_INSET_Y_PX;
        let track_height = (height - view::TERMINAL_SCROLLBAR_TRACK_INSET_Y_PX * 2.0).max(12.0);
        let thumb_height = (track_height
            * (visible_lines as f32 / total_lines as f32).clamp(0.08, 1.0))
        .max(view::TERMINAL_SCROLLBAR_MIN_THUMB_HEIGHT_PX)
        .min(track_height);
        let thumb_range = (track_height - thumb_height).max(0.0);
        let thumb_top = track_top
            + thumb_range
                * scrollbar_thumb_top_ratio(session.screen.display_offset, max_offset)
                    .clamp(0.0, 1.0);

        Some(TerminalScrollbarGeometry {
            left,
            top,
            width,
            height,
            track_top,
            track_height,
            thumb_top,
            thumb_height,
            max_offset,
        })
    }

    fn scrollbar_hit_test(
        &self,
        position: Point<Pixels>,
        geometry: TerminalScrollbarGeometry,
    ) -> bool {
        let x: f32 = position.x.into();
        let y: f32 = position.y.into();
        x >= geometry.left
            && x <= geometry.left + geometry.width
            && y >= geometry.top
            && y <= geometry.top + geometry.height
    }

    fn scrollbar_thumb_contains(
        &self,
        position: Point<Pixels>,
        geometry: TerminalScrollbarGeometry,
    ) -> bool {
        let y: f32 = position.y.into();
        y >= geometry.thumb_top && y <= geometry.thumb_top + geometry.thumb_height
    }

    fn scroll_terminal_from_scrollbar(
        &mut self,
        position: Point<Pixels>,
        geometry: TerminalScrollbarGeometry,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.terminal_scrollbar_drag.as_mut() else {
            return;
        };

        let thumb_top_ratio = scrollbar_ratio_for_position(position, geometry, drag.grab_offset_px);
        let display_offset =
            display_offset_for_scrollbar_ratio(thumb_top_ratio, geometry.max_offset);
        let ratio_changed = (drag.thumb_top_ratio - thumb_top_ratio).abs() > 0.0001;
        let offset_changed = drag.last_display_offset != display_offset;

        drag.thumb_top_ratio = thumb_top_ratio;
        drag.last_display_offset = display_offset;

        if offset_changed {
            self.pending_terminal_display_offset = Some(display_offset);
        }

        if ratio_changed || offset_changed {
            cx.notify();
        }
    }

    fn current_terminal_buffer_line(
        &self,
        session: &crate::terminal::session::TerminalSessionView,
    ) -> usize {
        session
            .screen
            .history_size
            .saturating_sub(session.screen.display_offset)
            .saturating_add(session.screen.rows / 2)
    }

    fn current_search_highlight(
        &self,
        session: Option<&crate::terminal::session::TerminalSessionView>,
    ) -> Option<view::TerminalSearchHighlight> {
        let session = session?;
        let selected_index = self.terminal_search.selected_index?;
        let selected = self.terminal_search.matches.get(selected_index)?;
        let top_buffer_line = session
            .screen
            .history_size
            .saturating_sub(session.screen.display_offset);
        let viewport_row = selected.buffer_line.checked_sub(top_buffer_line)?;
        (viewport_row < session.screen.rows).then_some(view::TerminalSearchHighlight {
            row: viewport_row,
            start_column: selected.start_column,
            end_column: selected.end_column,
        })
    }

    fn terminal_search_model(&self) -> Option<view::TerminalSearchUiModel> {
        self.terminal_search.active.then(|| {
            let summary = match (
                self.terminal_search.selected_index,
                self.terminal_search.matches.len(),
            ) {
                (Some(index), total) if total > 0 => format!("{} of {}", index + 1, total),
                _ => "no matches".to_string(),
            };
            view::TerminalSearchUiModel {
                query: self.terminal_search.query.clone(),
                summary,
                case_sensitive: self.terminal_search.case_sensitive,
            }
        })
    }

    fn sync_server_port_snapshot(&mut self, runtime: &RuntimeState, cx: &mut Context<Self>) {
        let tracked_ports = live_server_ports(&self.state, runtime);
        if tracked_ports.is_empty() {
            self.server_port_snapshot = ServerPortSnapshotState::default();
            return;
        }

        if self.server_port_snapshot.tracked_ports != tracked_ports {
            self.server_port_snapshot.tracked_ports = tracked_ports.clone();
            self.server_port_snapshot
                .statuses
                .retain(|port, _| tracked_ports.binary_search(port).is_ok());
            self.server_port_snapshot.last_checked_at = None;
        }

        let missing_status = tracked_ports
            .iter()
            .any(|port| !self.server_port_snapshot.statuses.contains_key(port));
        let should_refresh = missing_status
            || self
                .server_port_snapshot
                .last_checked_at
                .map(|checked_at| {
                    checked_at.elapsed()
                        >= server_port_refresh_interval(
                            runtime,
                            &self.state,
                            &self.server_port_snapshot,
                        )
                })
                .unwrap_or(true);
        if !should_refresh || self.server_port_snapshot.refresh_in_flight {
            return;
        }

        self.server_port_snapshot.refresh_in_flight = true;
        let ports = tracked_ports.clone();
        let background_executor = cx.background_executor().clone();
        let native_dialog_blockers = self.native_dialog_blockers.clone();
        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let mut async_cx = cx.clone();
                let native_dialog_blockers = native_dialog_blockers.clone();
                async move {
                    let statuses = background_executor
                        .spawn(async move { ports_service::snapshot_ports(&ports).ok() })
                        .await;
                    while native_dialog_blockers.load(Ordering::Acquire) > 0 {
                        background_executor.timer(Duration::from_millis(50)).await;
                    }
                    let _ = this.update(&mut async_cx, |this, cx: &mut Context<'_, Self>| {
                        this.server_port_snapshot.refresh_in_flight = false;
                        this.server_port_snapshot.last_checked_at = Some(Instant::now());
                        if let Some(statuses) = statuses {
                            this.server_port_snapshot.statuses = statuses;
                            let tracked_ports = this.server_port_snapshot.tracked_ports.clone();
                            this.server_port_snapshot
                                .statuses
                                .retain(|port, _| tracked_ports.binary_search(port).is_ok());
                        }
                        cx.notify();
                    });
                }
            },
        )
        .detach();
    }

    fn sync_active_port_state(&mut self, command_id: &str, port: Option<u16>) {
        let Some(port) = port else {
            self.active_port_state = None;
            return;
        };

        let state_needs_reset = self
            .active_port_state
            .as_ref()
            .map(|state| state.command_id != command_id || state.port != port)
            .unwrap_or(true);
        if state_needs_reset {
            self.active_port_state = Some(ActivePortState {
                command_id: command_id.to_string(),
                port,
                status: None,
                last_checked_at: None,
                kill_feedback: None,
                kill_feedback_until: None,
                refresh_in_flight: false,
            });
        }

        if let Some(state) = self.active_port_state.as_mut() {
            if state
                .kill_feedback_until
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                state.kill_feedback = None;
                state.kill_feedback_until = None;
            }
            state.status = self.server_port_snapshot.statuses.get(&port).cloned();
            state.last_checked_at = self.server_port_snapshot.last_checked_at;
            state.refresh_in_flight = self.server_port_snapshot.refresh_in_flight
                && self
                    .server_port_snapshot
                    .tracked_ports
                    .binary_search(&port)
                    .is_ok();
        }
    }

    fn refresh_port_state(&mut self, command_id: String, port: u16, cx: &mut Context<Self>) {
        let state = self
            .active_port_state
            .get_or_insert_with(|| ActivePortState {
                command_id: command_id.clone(),
                port,
                status: None,
                last_checked_at: None,
                kill_feedback: None,
                kill_feedback_until: None,
                refresh_in_flight: false,
            });
        state.command_id = command_id.clone();
        state.port = port;
        state.status = None;
        state.last_checked_at = None;
        state.refresh_in_flight = true;
        self.server_port_snapshot.statuses.remove(&port);
        self.server_port_snapshot.last_checked_at = None;
        self.server_port_snapshot.refresh_in_flight = false;
        self.sync_active_port_state(&command_id, Some(port));
        cx.notify();
    }

    fn invalidate_server_port_snapshot(&mut self, port: Option<u16>) {
        if let Some(port) = port {
            self.server_port_snapshot.statuses.remove(&port);
            if let Some(state) = self.active_port_state.as_mut() {
                if state.port == port {
                    state.status = None;
                    state.last_checked_at = None;
                    state.refresh_in_flight = true;
                }
            }
        }
        self.server_port_snapshot.last_checked_at = None;
        self.server_port_snapshot.refresh_in_flight = false;
    }

    fn record_port_kill_feedback(
        &mut self,
        command_id: &str,
        port: u16,
        feedback: PortKillFeedback,
    ) {
        let state = self
            .active_port_state
            .get_or_insert_with(|| ActivePortState {
                command_id: command_id.to_string(),
                port,
                status: None,
                last_checked_at: None,
                kill_feedback: None,
                kill_feedback_until: None,
                refresh_in_flight: false,
            });
        state.command_id = command_id.to_string();
        state.port = port;
        state.kill_feedback = Some(feedback);
        state.kill_feedback_until = Some(Instant::now() + std::time::Duration::from_secs(2));
        state.last_checked_at = None;
    }

    fn maybe_auto_submit_ssh_password(
        &mut self,
        active_session: Option<&crate::terminal::session::TerminalSessionView>,
    ) {
        let Some(session) = active_session else {
            self.ssh_password_prompt_state = None;
            return;
        };
        let Some(connection_id) = session
            .runtime
            .ssh_launch
            .as_ref()
            .map(|launch| launch.ssh_connection_id.as_str())
        else {
            self.ssh_password_prompt_state = None;
            return;
        };
        let Some(connection) = self.state.find_ssh_connection(connection_id) else {
            self.ssh_password_prompt_state = None;
            return;
        };
        let Some(prompt) = ssh_password_prompt(session, connection) else {
            self.ssh_password_prompt_state = None;
            return;
        };
        let Some(password) = connection
            .password
            .as_ref()
            .filter(|password| !password.is_empty())
        else {
            return;
        };

        let prompt_state = SshPasswordPromptState {
            session_id: session.runtime.session_id.clone(),
            fingerprint: prompt.fingerprint,
        };
        if self.ssh_password_prompt_state.as_ref() == Some(&prompt_state) {
            return;
        }

        if self.remote_mode.is_some() {
            self.remote_send_terminal_input(RemoteTerminalInput::Text {
                session_id: session.runtime.session_id.clone(),
                text: format!("{password}\r"),
            });
            self.ssh_password_prompt_state = Some(prompt_state);
            self.terminal_notice = Some("Sent saved SSH password.".to_string());
            return;
        }

        match self
            .process_manager
            .write_to_session(&session.runtime.session_id, &format!("{password}\r"))
        {
            Ok(()) => {
                self.ssh_password_prompt_state = Some(prompt_state);
                self.terminal_notice = Some("Sent saved SSH password.".to_string());
            }
            Err(error) => {
                self.terminal_notice = Some(format!("Failed to send SSH password: {error}"));
            }
        }
    }

    fn respond_to_ssh_prompt_action(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let Some(session) = self.current_session_view(session_id) else {
            self.terminal_notice = Some("SSH session is not available.".to_string());
            cx.notify();
            return;
        };
        let Some(connection_id) = session
            .runtime
            .ssh_launch
            .as_ref()
            .map(|launch| launch.ssh_connection_id.as_str())
        else {
            self.terminal_notice = Some("This terminal is not an SSH session.".to_string());
            cx.notify();
            return;
        };
        let Some(connection) = self.state.find_ssh_connection(connection_id) else {
            self.terminal_notice = Some(format!("Unknown SSH connection `{connection_id}`"));
            cx.notify();
            return;
        };
        if let Some(prompt) = ssh_password_prompt(&session, connection) {
            let Some(password) = connection
                .password
                .as_ref()
                .filter(|password| !password.is_empty())
            else {
                self.terminal_notice =
                    Some("No saved password for this SSH connection.".to_string());
                cx.notify();
                return;
            };
            self.ssh_password_prompt_state = Some(SshPasswordPromptState {
                session_id: session.runtime.session_id.clone(),
                fingerprint: prompt.fingerprint,
            });

            if self.remote_mode.is_some() {
                self.remote_send_terminal_input(RemoteTerminalInput::Text {
                    session_id: session_id.to_string(),
                    text: format!("{password}\r"),
                });
                self.terminal_notice = Some("Sent SSH password.".to_string());
                cx.notify();
                return;
            }

            match self
                .process_manager
                .write_to_session(session_id, &format!("{password}\r"))
            {
                Ok(()) => {
                    self.terminal_notice = Some("Sent SSH password.".to_string());
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Failed to send SSH password: {error}"));
                }
            }
            cx.notify();
            return;
        }

        if ssh_host_key_prompt(&session) {
            if self.remote_mode.is_some() {
                self.remote_send_terminal_input(RemoteTerminalInput::Text {
                    session_id: session_id.to_string(),
                    text: "yes\r".to_string(),
                });
                self.terminal_notice = Some("Sent `yes` to the SSH host check.".to_string());
                cx.notify();
                return;
            }

            match self.process_manager.write_to_session(session_id, "yes\r") {
                Ok(()) => {
                    self.terminal_notice = Some("Sent `yes` to the SSH host check.".to_string());
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Failed to send `yes`: {error}"));
                }
            }
            cx.notify();
            return;
        }

        self.terminal_notice = Some("No active SSH prompt to answer.".to_string());
        cx.notify();
    }

    fn runtime_controls_model(
        &mut self,
        active_tab_type: Option<TabType>,
        active_spec: &crate::state::ActiveTerminalSpec,
        active_session: Option<&crate::terminal::session::TerminalSessionView>,
    ) -> Option<view::TerminalRuntimeControlsModel> {
        let remote = self.remote_mode.is_some();
        let allow_mutation = !remote || self.remote_has_control();

        if active_tab_type == Some(TabType::Ssh) {
            self.active_port_state = None;
            let session = active_session?;
            let connection_id = session
                .runtime
                .ssh_launch
                .as_ref()
                .map(|launch| launch.ssh_connection_id.as_str())?;
            let connection = self.state.find_ssh_connection(connection_id)?;
            let password_prompt = ssh_password_prompt(session, connection);
            let host_key_prompt = ssh_host_key_prompt(session);
            let has_saved_password = connection
                .password
                .as_ref()
                .is_some_and(|password| !password.is_empty());

            let (port_label, prompt_action_label, prompt_action_color) =
                if password_prompt.is_some() {
                    (
                        Some("password prompt".to_string()),
                        has_saved_password.then_some("send password".to_string()),
                        if has_saved_password {
                            theme::PRIMARY
                        } else {
                            theme::TEXT_MUTED
                        },
                    )
                } else if host_key_prompt {
                    (
                        Some("verify host".to_string()),
                        Some("send yes".to_string()),
                        theme::WARNING_TEXT,
                    )
                } else {
                    return None;
                };

            return Some(view::TerminalRuntimeControlsModel {
                port_label,
                port_color: theme::WARNING_TEXT,
                can_start: false,
                can_stop: false,
                can_restart: false,
                can_clear: false,
                can_kill_port: false,
                can_open_url: false,
                kill_label: "kill",
                kill_color: theme::WARNING_TEXT,
                prompt_action_label,
                prompt_action_color,
                search_active: self.terminal_search.active,
                search_case_sensitive: self.terminal_search.case_sensitive,
                search_summary: None,
                can_search: active_session.is_some(),
                can_jump_prev_prompt: session.runtime.previous_prompt_line(None).is_some(),
                can_jump_next_prompt: false,
                can_export_screen: active_session.is_some(),
                can_export_scrollback: active_session.is_some(),
                can_export_selection: self.selection_range(session.screen.cols).is_some(),
                mouse_override_enabled: self.state.settings().terminal_mouse_override,
                read_only_enabled: self.state.settings().terminal_read_only,
            });
        }

        if active_tab_type != Some(TabType::Server) {
            self.active_port_state = None;
            let session = active_session?;
            let current_line = self.current_terminal_buffer_line(session);
            return Some(view::TerminalRuntimeControlsModel {
                port_label: None,
                port_color: theme::TEXT_DIM,
                can_start: false,
                can_stop: false,
                can_restart: false,
                can_clear: active_session.is_some() && !remote,
                can_kill_port: false,
                can_open_url: false,
                kill_label: "kill",
                kill_color: theme::WARNING_TEXT,
                prompt_action_label: None,
                prompt_action_color: theme::PRIMARY,
                search_active: self.terminal_search.active,
                search_case_sensitive: self.terminal_search.case_sensitive,
                search_summary: self.terminal_search_model().map(|search| search.summary),
                can_search: true,
                can_jump_prev_prompt: session
                    .runtime
                    .previous_prompt_line(Some(current_line))
                    .is_some(),
                can_jump_next_prompt: session
                    .runtime
                    .next_prompt_line(Some(current_line))
                    .is_some(),
                can_export_screen: true,
                can_export_scrollback: true,
                can_export_selection: self.selection_range(session.screen.cols).is_some(),
                mouse_override_enabled: self.state.settings().terminal_mouse_override,
                read_only_enabled: self.state.settings().terminal_read_only,
            });
        }

        let (command_id, port) = {
            let lookup = self.state.find_command(&active_spec.session_id)?;
            (lookup.command.id.clone(), lookup.command.port)
        };
        let remote_url_available = remote
            && port
                .and_then(|value| self.remote_port_forward_state(value))
                .is_some_and(|state| state.listener_active);
        let remote_forward_state = if remote {
            port.and_then(|value| self.remote_port_forward_state(value))
        } else {
            None
        };
        let port_status = if remote {
            self.active_port_state = None;
            port.and_then(|port| self.current_port_statuses().get(&port).cloned())
        } else {
            self.sync_active_port_state(&command_id, port);
            self.active_port_state
                .as_ref()
                .filter(|state| state.command_id == command_id)
                .and_then(|state| state.status.clone())
        };

        let status = active_session
            .map(|session| session.runtime.status)
            .unwrap_or(crate::state::SessionStatus::Stopped);
        let port_state = self
            .active_port_state
            .as_ref()
            .filter(|state| state.command_id == command_id);
        let has_port_conflict = port_status
            .as_ref()
            .map(|status| !is_managed_port_owner(active_session, status))
            .unwrap_or(false);
        let probe_disagrees_with_live_session = active_session
            .is_some_and(|session| session.runtime.status.is_live())
            && port_status.as_ref().is_some_and(|status| !status.in_use);
        let port_label = port.map(|port| {
            if let Some(status) = port_status.as_ref() {
                let base = if probe_disagrees_with_live_session {
                    format!("port {port} • probing")
                } else if status.in_use {
                    if is_managed_port_owner(active_session, status) {
                        format!("port {port} • live")
                    } else {
                        let owner = status
                            .process_name
                            .clone()
                            .unwrap_or_else(|| "external process".to_string());
                        match status.pid {
                            Some(pid) => format!("port {port} • {owner} ({pid})"),
                            None => format!("port {port} • {owner}"),
                        }
                    }
                } else {
                    format!("port {port} • free")
                };

                if let Some(forward_state) = remote_forward_state.as_ref() {
                    if forward_state.listener_active {
                        format!("{base} • mirrored locally")
                    } else if forward_state.local_port_busy {
                        format!("{base} • local port busy")
                    } else {
                        base
                    }
                } else {
                    base
                }
            } else {
                format!("port {port} • checking")
            }
        });
        let port_color = if has_port_conflict {
            theme::WARNING_TEXT
        } else if probe_disagrees_with_live_session {
            theme::TEXT_MUTED
        } else if port_status
            .as_ref()
            .map(|status| status.in_use)
            .unwrap_or(false)
        {
            theme::SUCCESS_TEXT
        } else {
            theme::TEXT_DIM
        };
        let (kill_label, kill_color) = match port_state.and_then(|state| state.kill_feedback) {
            Some(PortKillFeedback::Killed) => ("freed", theme::SUCCESS_TEXT),
            Some(PortKillFeedback::None) => ("none", theme::TEXT_MUTED),
            Some(PortKillFeedback::Error) => ("error", theme::DANGER_TEXT),
            None => ("kill", theme::WARNING_TEXT),
        };

        Some(view::TerminalRuntimeControlsModel {
            port_label,
            port_color,
            can_start: allow_mutation && !status.is_live(),
            can_stop: allow_mutation && status.is_live(),
            can_restart: allow_mutation && status.is_live(),
            can_clear: active_session.is_some() && !remote,
            can_kill_port: !remote && port.is_some() && has_port_conflict,
            can_open_url: (remote && remote_url_available)
                || (!remote
                    && port.is_some()
                    && status == crate::state::SessionStatus::Running
                    && !has_port_conflict),
            kill_label,
            kill_color,
            prompt_action_label: None,
            prompt_action_color: theme::PRIMARY,
            search_active: self.terminal_search.active,
            search_case_sensitive: self.terminal_search.case_sensitive,
            search_summary: self.terminal_search_model().map(|search| search.summary),
            can_search: active_session.is_some(),
            can_jump_prev_prompt: active_session
                .map(|session| {
                    session
                        .runtime
                        .previous_prompt_line(Some(self.current_terminal_buffer_line(session)))
                        .is_some()
                })
                .unwrap_or(false),
            can_jump_next_prompt: active_session
                .map(|session| {
                    session
                        .runtime
                        .next_prompt_line(Some(self.current_terminal_buffer_line(session)))
                        .is_some()
                })
                .unwrap_or(false),
            can_export_screen: active_session.is_some(),
            can_export_scrollback: active_session.is_some(),
            can_export_selection: active_session
                .and_then(|session| self.selection_range(session.screen.cols))
                .is_some(),
            mouse_override_enabled: self.state.settings().terminal_mouse_override,
            read_only_enabled: self.state.settings().terminal_read_only,
        })
    }

    fn toggle_terminal_search_action(&mut self, cx: &mut Context<Self>) {
        if self.terminal_search.active {
            self.terminal_search = TerminalSearchState::default();
        } else {
            self.terminal_search.active = true;
            self.refresh_terminal_search_results(cx);
        }
        cx.notify();
    }

    fn close_terminal_search_action(&mut self, cx: &mut Context<Self>) {
        self.terminal_search = TerminalSearchState::default();
        cx.notify();
    }

    fn toggle_terminal_search_case_action(&mut self, cx: &mut Context<Self>) {
        self.terminal_search.case_sensitive = !self.terminal_search.case_sensitive;
        self.refresh_terminal_search_results(cx);
    }

    fn refresh_terminal_search_results(&mut self, cx: &mut Context<Self>) {
        let active_session = self.current_active_session_view();
        let Some(session_id) = self.resolved_terminal_session_id(active_session.as_ref()) else {
            self.terminal_search.matches.clear();
            self.terminal_search.selected_index = None;
            if self.remote_mode.is_some() {
                self.terminal_notice = Some(
                    "Remote terminal is still starting. Wait a moment and try again.".to_string(),
                );
            }
            cx.notify();
            return;
        };
        let query = self.terminal_search.query.clone();
        self.terminal_search.matches = if self.remote_mode.is_some() {
            match self.remote_request(RemoteAction::SearchSession {
                session_id,
                query,
                case_sensitive: self.terminal_search.case_sensitive,
            }) {
                Ok(RemoteActionResult {
                    ok: true,
                    payload: Some(RemoteActionPayload::SearchMatches { matches }),
                    ..
                }) => matches,
                Ok(result) => {
                    self.terminal_notice = Some(result.message.unwrap_or_else(|| {
                        "Could not search the remote terminal buffer.".to_string()
                    }));
                    Vec::new()
                }
                Err(error) => {
                    self.terminal_notice = Some(format!(
                        "Could not search the remote terminal buffer: {error}"
                    ));
                    Vec::new()
                }
            }
        } else {
            self.process_manager
                .search_session(
                    &session_id,
                    &query,
                    self.terminal_search.case_sensitive,
                    256,
                )
                .unwrap_or_default()
        };
        self.terminal_search.selected_index =
            (!self.terminal_search.matches.is_empty()).then_some(0);
        if self.terminal_search.selected_index.is_some() {
            self.jump_to_selected_search_match();
        }
        cx.notify();
    }

    fn jump_to_selected_search_match(&mut self) {
        let Some(index) = self.terminal_search.selected_index else {
            return;
        };
        let Some(found) = self.terminal_search.matches.get(index) else {
            return;
        };
        let active_session = self.current_active_session_view();
        let Some(session_id) = self.resolved_terminal_session_id(active_session.as_ref()) else {
            return;
        };
        if self.remote_mode.is_some() {
            self.remote_send_action(RemoteAction::ScrollSessionToBufferLine {
                session_id,
                buffer_line: found.buffer_line,
            });
            return;
        }
        let _ = self
            .process_manager
            .scroll_session_to_buffer_line(&session_id, found.buffer_line);
    }

    fn cycle_terminal_search_match(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.terminal_search.matches.is_empty() {
            return;
        }

        let len = self.terminal_search.matches.len();
        let next_index = match self.terminal_search.selected_index {
            Some(current) if forward => (current + 1) % len,
            Some(current) => (current + len - 1) % len,
            None => 0,
        };
        self.terminal_search.selected_index = Some(next_index);
        self.jump_to_selected_search_match();
        cx.notify();
    }

    fn jump_terminal_prompt(&mut self, previous: bool, cx: &mut Context<Self>) {
        let Some(session) = self.current_active_session_view() else {
            return;
        };
        let current_line = self.current_terminal_buffer_line(&session);
        let target = if previous {
            session.runtime.previous_prompt_line(Some(current_line))
        } else {
            session.runtime.next_prompt_line(Some(current_line))
        };
        let Some(target) = target else {
            return;
        };
        if self.remote_mode.is_some() {
            self.remote_send_action(RemoteAction::ScrollSessionToBufferLine {
                session_id: session.runtime.session_id.clone(),
                buffer_line: target,
            });
            cx.notify();
            return;
        }
        if self
            .process_manager
            .scroll_session_to_buffer_line(&session.runtime.session_id, target)
            .is_ok()
        {
            cx.notify();
        }
    }

    fn toggle_terminal_mouse_override_action(&mut self, cx: &mut Context<Self>) {
        let mut settings = self.state.settings().clone();
        settings.terminal_mouse_override = !settings.terminal_mouse_override;
        self.state.update_settings(settings);
        self.save_config_state();
        cx.notify();
    }

    fn toggle_terminal_read_only_action(&mut self, cx: &mut Context<Self>) {
        let mut settings = self.state.settings().clone();
        settings.terminal_read_only = !settings.terminal_read_only;
        self.state.update_settings(settings);
        self.save_config_state();
        self.terminal_notice = Some(if self.state.settings().terminal_read_only {
            "Terminal input is now read-only.".to_string()
        } else {
            "Terminal input is live again.".to_string()
        });
        cx.notify();
    }

    fn export_terminal_view_action(
        &mut self,
        include_scrollback: bool,
        selection_only: bool,
        cx: &mut Context<Self>,
    ) {
        let active_session = self.current_active_session_view();
        let Some(session_id) = self.resolved_terminal_session_id(active_session.as_ref()) else {
            if self.remote_mode.is_some() {
                self.terminal_notice = Some(
                    "Remote terminal is still starting. Wait a moment and try again.".to_string(),
                );
            }
            cx.notify();
            return;
        };
        let kind = if selection_only {
            "selection"
        } else if include_scrollback {
            "scrollback"
        } else {
            "screen"
        };
        let text = if self.remote_mode.is_some() {
            let export = if selection_only {
                RemoteTerminalExport::Selection {
                    text: self.selected_text().unwrap_or_default(),
                }
            } else if include_scrollback {
                RemoteTerminalExport::Scrollback
            } else {
                RemoteTerminalExport::Screen
            };
            match self.remote_request(RemoteAction::ExportSessionText { session_id, export }) {
                Ok(RemoteActionResult {
                    ok: true,
                    payload: Some(RemoteActionPayload::ExportText { text }),
                    ..
                }) => text,
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| format!("Failed to export terminal {kind}.")),
                    );
                    cx.notify();
                    return;
                }
                Err(error) => {
                    self.terminal_notice =
                        Some(format!("Failed to export terminal {kind}: {error}"));
                    cx.notify();
                    return;
                }
            }
        } else if selection_only {
            self.selected_text().unwrap_or_default()
        } else if include_scrollback {
            self.process_manager
                .session_scrollback_text(&session_id)
                .unwrap_or_default()
        } else {
            self.process_manager
                .session_screen_text(&session_id)
                .unwrap_or_default()
        };

        if text.is_empty() {
            self.terminal_notice = Some("Nothing to export from this terminal.".to_string());
            cx.notify();
            return;
        }

        match write_terminal_export(kind, &text) {
            Ok(path) => {
                self.terminal_notice = Some(format!("Wrote terminal {kind} to {}", path.display()));
            }
            Err(error) => {
                self.terminal_notice = Some(format!("Failed to export terminal {kind}: {error}"));
            }
        }
        cx.notify();
    }

    fn start_server_action(
        &mut self,
        command_id: &str,
        focus_started_server: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            let dimensions = self.terminal_dimensions(window);
            match self.remote_request(RemoteAction::StartServer {
                command_id: command_id.to_string(),
                focus: focus_started_server,
                dimensions,
            }) {
                Ok(result) if result.ok => {
                    if focus_started_server {
                        self.select_server_tab_action(command_id, cx);
                    }
                    self.terminal_notice = None;
                }
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Failed to start remote server.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Failed to start remote server: {error}"));
                }
            }
            cx.notify();
            return;
        }

        let dimensions = self.terminal_dimensions(window);
        let Some(port) = self
            .state
            .find_command(command_id)
            .and_then(|lookup| lookup.command.port)
        else {
            let result = if focus_started_server {
                self.process_manager
                    .start_server(&mut self.state, command_id, dimensions)
            } else {
                self.process_manager.start_server_in_background(
                    &mut self.state,
                    command_id,
                    dimensions,
                )
            };
            match result {
                Ok(()) => {
                    if focus_started_server {
                        self.synced_session_id = Some(command_id.to_string());
                    }
                    self.terminal_notice = None;
                    self.save_session_state();
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Failed to start server: {error}"));
                }
            }
            cx.notify();
            return;
        };

        self.invalidate_server_port_snapshot(Some(port));
        if let Some(state) = self.active_port_state.as_mut() {
            if state.command_id == command_id && state.port == port {
                state.status = None;
                state.last_checked_at = None;
                state.refresh_in_flight = true;
            }
        }

        let command_id = command_id.to_string();
        let background_executor = cx.background_executor().clone();
        let native_dialog_blockers = self.native_dialog_blockers.clone();
        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let mut async_cx = cx.clone();
                let native_dialog_blockers = native_dialog_blockers.clone();
                async move {
                    let status = background_executor
                        .spawn(async move { ports_service::check_port_in_use(port).ok() })
                        .await;
                    while native_dialog_blockers.load(Ordering::Acquire) > 0 {
                        background_executor.timer(Duration::from_millis(50)).await;
                    }
                    let _ = this.update(&mut async_cx, |this, cx: &mut Context<'_, Self>| {
                        if let Some(state) = this.active_port_state.as_mut() {
                            if state.command_id == command_id && state.port == port {
                                state.status = status.clone();
                                state.last_checked_at = Some(Instant::now());
                                state.refresh_in_flight = false;
                            }
                        }

                        if let Some(status) = status.filter(|status| status.in_use) {
                            let owner = status
                                .process_name
                                .clone()
                                .unwrap_or_else(|| "another process".to_string());
                            let owner_label = status
                                .pid
                                .map(|pid| format!("{owner} ({pid})"))
                                .unwrap_or(owner);
                            this.terminal_notice =
                                Some(format!("Port {port} is already in use by {owner_label}."));
                            cx.notify();
                            return;
                        }

                        let result = if focus_started_server {
                            this.process_manager.start_server(
                                &mut this.state,
                                &command_id,
                                dimensions,
                            )
                        } else {
                            this.process_manager.start_server_in_background(
                                &mut this.state,
                                &command_id,
                                dimensions,
                            )
                        };

                        match result {
                            Ok(()) => {
                                if focus_started_server {
                                    this.synced_session_id = Some(command_id.clone());
                                }
                                this.terminal_notice = None;
                                this.save_session_state();
                            }
                            Err(error) => {
                                this.terminal_notice =
                                    Some(format!("Failed to start server: {error}"));
                            }
                        }
                        cx.notify();
                    });
                }
            },
        )
        .detach();
        if self.terminal_notice.is_none() {
            self.terminal_notice = Some(format!("Checking port {port} before starting..."));
        }
        cx.notify();
    }

    fn stop_server_action(&mut self, command_id: &str, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            self.remote_send_action(RemoteAction::StopServer {
                command_id: command_id.to_string(),
            });
            self.terminal_notice = Some(format!("Stopping remote `{command_id}`..."));
            cx.notify();
            return;
        }

        let port = self
            .state
            .find_command(command_id)
            .and_then(|lookup| lookup.command.port);
        self.invalidate_server_port_snapshot(port);
        let command_id = command_id.to_string();
        if let Some(state) = self.active_port_state.as_mut() {
            if state.command_id == command_id {
                state.status = None;
                state.last_checked_at = None;
                state.refresh_in_flight = true;
            }
        }
        self.terminal_notice = Some(format!("Stopping `{command_id}`..."));

        let process_manager = self.process_manager.clone();
        let native_dialog_blockers = self.native_dialog_blockers.clone();
        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let background_executor = cx.background_executor().clone();
                let mut async_cx = cx.clone();
                let native_dialog_blockers = native_dialog_blockers.clone();
                async move {
                    let command_id_for_wait = command_id.clone();
                    let stopped = background_executor
                        .spawn(async move {
                            process_manager.stop_server_and_wait(
                                &command_id_for_wait,
                                std::time::Duration::from_secs(5),
                            )
                        })
                        .await;

                    while native_dialog_blockers.load(Ordering::Acquire) > 0 {
                        background_executor.timer(Duration::from_millis(50)).await;
                    }
                    let _ = this.update(&mut async_cx, |this, cx: &mut Context<'_, Self>| {
                        if let Some(state) = this.active_port_state.as_mut() {
                            if state.command_id == command_id {
                                state.status = None;
                                state.last_checked_at = None;
                                state.refresh_in_flight = false;
                            }
                        }

                        if stopped {
                            this.terminal_notice =
                                Some(format!("Stopped `{command_id}` and released its processes."));
                        } else {
                            this.terminal_notice = Some(format!(
                                "Failed to stop `{command_id}` cleanly. The port may still be in use."
                            ));
                        }
                        cx.notify();
                    });
                }
            },
        )
        .detach();
        cx.notify();
    }

    fn restart_server_action(
        &mut self,
        command_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            let dimensions = self.terminal_dimensions(window);
            match self.remote_request(RemoteAction::RestartServer {
                command_id: command_id.to_string(),
                dimensions,
            }) {
                Ok(result) if result.ok => {
                    self.terminal_notice = None;
                }
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Failed to restart remote server.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice =
                        Some(format!("Failed to restart remote server: {error}"));
                }
            }
            cx.notify();
            return;
        }

        let dimensions = self.terminal_dimensions(window);
        let port = self
            .state
            .find_command(command_id)
            .and_then(|lookup| lookup.command.port);
        self.invalidate_server_port_snapshot(port);
        if let Err(error) =
            self.process_manager
                .restart_server(&mut self.state, command_id, dimensions)
        {
            self.terminal_notice = Some(format!("Failed to restart server: {error}"));
        }
        cx.notify();
    }

    fn clear_server_output_action(&mut self, command_id: &str, cx: &mut Context<Self>) {
        if let Err(error) = self.process_manager.clear_virtual_output(command_id) {
            self.terminal_notice = Some(format!("Failed to clear output: {error}"));
        } else {
            self.terminal_notice = Some("Cleared terminal output.".to_string());
        }
        cx.notify();
    }

    fn open_server_url_action(&mut self, command_id: &str, cx: &mut Context<Self>) {
        let Some(port) = self
            .state
            .find_command(command_id)
            .and_then(|lookup| lookup.command.port)
        else {
            self.terminal_notice = Some("This command does not define a local port.".to_string());
            cx.notify();
            return;
        };

        if self.remote_mode.is_some() {
            match self.remote_port_forward_state(port) {
                Some(state) if state.listener_active => {}
                Some(state) => {
                    self.terminal_notice = Some(
                        state.message.unwrap_or_else(|| {
                            format!(
                                "Could not open localhost:{port} because this client is not forwarding that host port."
                            )
                        }),
                    );
                    cx.notify();
                    return;
                }
                None => {
                    self.terminal_notice = Some(format!(
                        "Could not open localhost:{port} because the host server is not currently mirrored onto this client."
                    ));
                    cx.notify();
                    return;
                }
            }
        }

        let url = format!("http://localhost:{port}");
        match platform_service::open_url(&url) {
            Ok(()) => self.terminal_notice = Some(format!("Opened {url}")),
            Err(error) => self.terminal_notice = Some(format!("Failed to open {url}: {error}")),
        }
        cx.notify();
    }

    fn kill_server_port_action(
        &mut self,
        command_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(lookup) = self
            .state
            .find_command(command_id)
            .map(|lookup| (lookup.project.id.clone(), lookup.command.clone()))
        else {
            self.terminal_notice = Some(format!("Unknown command `{command_id}`"));
            cx.notify();
            return;
        };
        let (project_id, command) = lookup;
        let Some(port) = command.port else {
            self.terminal_notice = Some("This command does not define a port.".to_string());
            cx.notify();
            return;
        };

        let _ = self.process_manager.write_virtual_text(
            command_id,
            &format!("\r\n\x1b[33m--- Resolving port {port} conflict... ---\x1b[0m\r\n"),
        );

        let is_active = self
            .process_manager
            .runtime_state()
            .sessions
            .get(command_id)
            .map(|session| session.status.is_live())
            .unwrap_or(false);
        if is_active
            && !self
                .process_manager
                .stop_server_and_wait(command_id, std::time::Duration::from_secs(5))
        {
            self.record_port_kill_feedback(command_id, port, PortKillFeedback::Error);
            self.terminal_notice = Some(format!(
                "Managed process `{command_id}` did not stop cleanly."
            ));
            cx.notify();
            return;
        }

        let feedback = match ports_service::kill_port(port) {
            Ok(()) => PortKillFeedback::Killed,
            Err(error) if error.contains("No process found") => PortKillFeedback::None,
            Err(error) => {
                self.record_port_kill_feedback(command_id, port, PortKillFeedback::Error);
                self.terminal_notice = Some(format!("Failed to free port {port}: {error}"));
                let _ = self.process_manager.write_virtual_text(
                    command_id,
                    &format!("\x1b[31mFailed to resolve port {port} conflict: {error}\x1b[0m\r\n"),
                );
                cx.notify();
                return;
            }
        };

        self.record_port_kill_feedback(command_id, port, feedback);
        self.refresh_port_state(command_id.to_string(), port, cx);
        let dimensions = self.terminal_dimensions(window);

        match self.process_manager.restart_server_with_banner(
            &mut self.state,
            command_id,
            dimensions,
            &format!("--- Starting after freeing port {port}... ---"),
        ) {
            Ok(()) => {
                self.synced_session_id = Some(command_id.to_string());
                self.terminal_notice = None;
                self.save_session_state();
            }
            Err(error) => {
                self.terminal_notice = Some(format!(
                    "Failed to restart server after freeing port: {error}"
                ));
                let _ = self.process_manager.write_virtual_text(
                    command_id,
                    &format!("\x1b[31mFailed to restart after freeing port: {error}\x1b[0m\r\n"),
                );
            }
        }
        let _ = project_id;
        let _ = command;
        cx.notify();
    }

    fn select_server_tab_action(&mut self, command_id: &str, cx: &mut Context<Self>) {
        if self.remote_mode.is_some() {
            let lookup = self.state.find_command(command_id).map(|lookup| {
                (
                    lookup.project.id.clone(),
                    lookup.command.id.clone(),
                    lookup.command.label.clone(),
                )
            });
            if let Some((project_id, command_id, label)) = lookup {
                self.state
                    .open_server_tab(&project_id, &command_id, Some(label));
            } else {
                self.state.select_tab(command_id);
            }
            self.show_terminal_surface();
            self.synced_session_id = Some(command_id.to_string());
            if let Some(remote_mode) = self.remote_mode.as_ref() {
                remote_mode
                    .client
                    .set_focused_session(Some(command_id.to_string()));
            }
            cx.notify();
            return;
        }

        let lookup = self.state.find_command(command_id).map(|lookup| {
            (
                lookup.project.id.clone(),
                lookup.command.id.clone(),
                lookup.command.label.clone(),
            )
        });

        if let Some((project_id, command_id, label)) = lookup {
            self.state
                .open_server_tab(&project_id, &command_id, Some(label));
        } else {
            self.state.select_tab(command_id);
        }
        self.show_terminal_surface();
        self.synced_session_id = Some(command_id.to_string());
        self.save_session_state();
        cx.notify();
    }

    fn launch_ai_action(
        &mut self,
        project_id: &str,
        tab_type: TabType,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            let dimensions = self.terminal_dimensions(window);
            match self.remote_request(RemoteAction::LaunchAi {
                project_id: project_id.to_string(),
                tab_type,
                dimensions,
            }) {
                Ok(result) if result.ok => match result.payload {
                    Some(RemoteActionPayload::AiTab {
                        tab_id,
                        project_id,
                        tab_type,
                        session_id,
                        label,
                        session_view,
                    }) => {
                        self.apply_remote_ai_tab(
                            &project_id,
                            tab_type,
                            &tab_id,
                            &session_id,
                            label,
                            session_view,
                        );
                    }
                    _ => {
                        self.show_terminal_surface();
                        self.terminal_notice = None;
                    }
                },
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Failed to launch AI session.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Failed to launch AI session: {error}"));
                }
            }
            cx.notify();
            return;
        }

        let dimensions = self.terminal_dimensions(window);
        match self.process_manager.start_ai_session(
            &mut self.state,
            project_id,
            tab_type,
            dimensions,
        ) {
            Ok(session_id) => {
                self.show_terminal_surface();
                self.synced_session_id = Some(session_id);
                self.terminal_notice = None;
                self.save_session_state();
            }
            Err(error) => {
                self.terminal_notice = Some(format!("Failed to launch AI session: {error}"));
            }
        }
        cx.notify();
    }

    fn select_ai_tab_action(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.remote_mode.is_some() {
            if let Some(tab) = self.state.find_ai_tab(tab_id).cloned() {
                self.state.select_tab(tab_id);
                self.show_terminal_surface();
                self.synced_session_id = tab.pty_session_id.clone();
                self.last_dimensions = None;
                if let Some(session_id) = tab.pty_session_id {
                    if let Some(remote_mode) = self.remote_mode.as_mut() {
                        remote_mode.client.set_focused_session(Some(session_id));
                    }
                }
                cx.notify();
                return;
            }
            if !self.ensure_mutation_control(cx) {
                return;
            }
            let dimensions = self.terminal_dimensions(window);
            match self.remote_request(RemoteAction::OpenAiTab {
                tab_id: tab_id.to_string(),
                dimensions,
            }) {
                Ok(result) if result.ok => match result.payload {
                    Some(RemoteActionPayload::AiTab {
                        tab_id,
                        project_id,
                        tab_type,
                        session_id,
                        label,
                        session_view,
                    }) => {
                        self.apply_remote_ai_tab(
                            &project_id,
                            tab_type,
                            &tab_id,
                            &session_id,
                            label,
                            session_view,
                        );
                    }
                    _ => {
                        self.show_terminal_surface();
                        self.terminal_notice = None;
                    }
                },
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Failed to open AI tab.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Failed to open AI tab: {error}"));
                }
            }
            cx.notify();
            return;
        }

        if !self.ensure_mutation_control(cx) {
            return;
        }
        let dimensions = self.terminal_dimensions(window);
        match self.process_manager.ensure_ai_session_for_tab(
            &mut self.state,
            tab_id,
            dimensions,
            true,
            false,
        ) {
            Ok(session_id) => {
                self.show_terminal_surface();
                self.synced_session_id = Some(session_id);
                self.terminal_notice = None;
                self.save_session_state();
            }
            Err(error) => {
                self.terminal_notice = Some(format!("Failed to open AI tab: {error}"));
            }
        }
        cx.notify();
    }

    fn restart_ai_tab_action(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            let dimensions = self.terminal_dimensions(window);
            match self.remote_request(RemoteAction::RestartAiTab {
                tab_id: tab_id.to_string(),
                dimensions,
            }) {
                Ok(result) if result.ok => match result.payload {
                    Some(RemoteActionPayload::AiTab {
                        tab_id,
                        project_id,
                        tab_type,
                        session_id,
                        label,
                        session_view,
                    }) => {
                        self.apply_remote_ai_tab(
                            &project_id,
                            tab_type,
                            &tab_id,
                            &session_id,
                            label,
                            session_view,
                        );
                    }
                    _ => {
                        self.show_terminal_surface();
                        self.terminal_notice = None;
                    }
                },
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Failed to restart AI tab.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Failed to restart AI tab: {error}"));
                }
            }
            cx.notify();
            return;
        }

        let dimensions = self.terminal_dimensions(window);
        match self
            .process_manager
            .restart_ai_session(&mut self.state, tab_id, dimensions)
        {
            Ok(session_id) => {
                self.show_terminal_surface();
                self.synced_session_id = Some(session_id);
                self.terminal_notice = None;
                self.save_session_state();
            }
            Err(error) => {
                self.terminal_notice = Some(format!("Failed to restart AI tab: {error}"));
            }
        }
        cx.notify();
    }

    fn close_ai_tab_action(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            match self.remote_request(RemoteAction::CloseAiTab {
                tab_id: tab_id.to_string(),
            }) {
                Ok(result) if result.ok => {
                    self.state.remove_tab(tab_id);
                    self.synced_session_id = None;
                    self.last_dimensions = None;
                }
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Failed to close AI tab.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Failed to close AI tab: {error}"));
                }
            }
            cx.notify();
            return;
        }

        if let Err(error) = self
            .process_manager
            .close_ai_session(&mut self.state, tab_id)
        {
            self.terminal_notice = Some(format!("Failed to close AI tab: {error}"));
        } else {
            self.synced_session_id = None;
            self.last_dimensions = None;
            self.save_session_state();
        }
        cx.notify();
    }

    fn open_ssh_tab_action(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        if self.remote_mode.is_some() {
            if let Some(tab) = self
                .state
                .find_ssh_tab_by_connection(connection_id)
                .cloned()
            {
                self.state.select_tab(&tab.id);
                self.show_terminal_surface();
                self.synced_session_id = tab.pty_session_id.clone();
                self.last_dimensions = None;
                if let Some(session_id) = tab.pty_session_id {
                    if let Some(remote_mode) = self.remote_mode.as_mut() {
                        remote_mode.client.set_focused_session(Some(session_id));
                    }
                }
                cx.notify();
                return;
            }
            if !self.ensure_mutation_control(cx) {
                return;
            }
            match self.remote_request(RemoteAction::OpenSshTab {
                connection_id: connection_id.to_string(),
            }) {
                Ok(result) if result.ok => {
                    self.show_terminal_surface();
                    self.terminal_notice = None;
                }
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Failed to open SSH tab.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Failed to open SSH tab: {error}"));
                }
            }
            cx.notify();
            return;
        }

        if !self.ensure_mutation_control(cx) {
            return;
        }
        let connection = self.state.find_ssh_connection(connection_id).cloned();
        let Some(connection) = connection else {
            self.terminal_notice = Some(format!("Unknown SSH connection `{connection_id}`"));
            cx.notify();
            return;
        };

        let project_id = self
            .state
            .find_ssh_tab_by_connection(connection_id)
            .map(|tab| tab.project_id.clone())
            .or_else(|| {
                self.state
                    .active_project()
                    .map(|project| project.id.clone())
            })
            .or_else(|| {
                self.state
                    .projects()
                    .first()
                    .map(|project| project.id.clone())
            })
            .unwrap_or_default();
        let tab_id = self
            .state
            .open_ssh_tab(&project_id, connection_id, Some(connection.label));

        self.show_terminal_surface();
        self.synced_session_id = self
            .state
            .find_ssh_tab(&tab_id)
            .and_then(|tab| tab.pty_session_id.clone());
        self.last_dimensions = None;
        self.save_session_state();
        cx.notify();
    }

    fn connect_ssh_action(
        &mut self,
        connection_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            let dimensions = self.terminal_dimensions(window);
            match self.remote_request(RemoteAction::ConnectSsh {
                connection_id: connection_id.to_string(),
                dimensions,
            }) {
                Ok(result) if result.ok => {
                    self.show_terminal_surface();
                    self.terminal_notice = None;
                }
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Failed to connect SSH session.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Failed to connect SSH session: {error}"));
                }
            }
            cx.notify();
            return;
        }

        let dimensions = self.terminal_dimensions(window);
        match self
            .process_manager
            .start_ssh_session(&mut self.state, connection_id, dimensions)
        {
            Ok(session_id) => {
                self.show_terminal_surface();
                self.synced_session_id = Some(session_id);
                self.last_dimensions = None;
                self.terminal_notice = None;
                self.save_session_state();
            }
            Err(error) => {
                self.terminal_notice = Some(format!("Failed to connect SSH session: {error}"));
            }
        }
        cx.notify();
    }

    fn restart_ssh_action(
        &mut self,
        connection_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            let dimensions = self.terminal_dimensions(window);
            match self.remote_request(RemoteAction::RestartSsh {
                connection_id: connection_id.to_string(),
                dimensions,
            }) {
                Ok(result) if result.ok => {
                    self.show_terminal_surface();
                    self.terminal_notice = None;
                }
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Failed to restart SSH session.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice = Some(format!("Failed to restart SSH session: {error}"));
                }
            }
            cx.notify();
            return;
        }

        let Some(tab_id) = self
            .state
            .find_ssh_tab_by_connection(connection_id)
            .map(|tab| tab.id.clone())
        else {
            self.connect_ssh_action(connection_id, window, cx);
            return;
        };

        let dimensions = self.terminal_dimensions(window);
        match self
            .process_manager
            .restart_ssh_session(&mut self.state, &tab_id, dimensions)
        {
            Ok(session_id) => {
                self.show_terminal_surface();
                self.synced_session_id = Some(session_id);
                self.last_dimensions = None;
                self.terminal_notice = None;
                self.save_session_state();
            }
            Err(error) => {
                self.terminal_notice = Some(format!("Failed to restart SSH session: {error}"));
            }
        }
        cx.notify();
    }

    fn disconnect_ssh_action(&mut self, connection_id: &str, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.remote_mode.is_some() {
            match self.remote_request(RemoteAction::DisconnectSsh {
                connection_id: connection_id.to_string(),
            }) {
                Ok(result) if result.ok => {
                    self.synced_session_id = None;
                    self.last_dimensions = None;
                    self.terminal_notice =
                        Some("SSH session is disconnected. Connect from the sidebar.".to_string());
                }
                Ok(result) => {
                    self.terminal_notice = Some(
                        result
                            .message
                            .unwrap_or_else(|| "Failed to disconnect SSH session.".to_string()),
                    );
                }
                Err(error) => {
                    self.terminal_notice =
                        Some(format!("Failed to disconnect SSH session: {error}"));
                }
            }
            cx.notify();
            return;
        }

        let Some(tab_id) = self
            .state
            .find_ssh_tab_by_connection(connection_id)
            .map(|tab| tab.id.clone())
        else {
            cx.notify();
            return;
        };

        if let Err(error) = self
            .process_manager
            .close_ssh_session(&mut self.state, &tab_id)
        {
            self.terminal_notice = Some(format!("Failed to disconnect SSH session: {error}"));
        } else {
            self.synced_session_id = None;
            self.last_dimensions = None;
            self.terminal_notice =
                Some("SSH session is disconnected. Connect from the sidebar.".to_string());
            self.save_session_state();
        }
        cx.notify();
    }

    fn handle_terminal_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.add_project_wizard.is_some() {
            return;
        }
        self.focus_terminal(window);
        self.terminal_scroll_px = px(0.0);

        if self.handle_terminal_scrollbar_mouse_down(event, window, cx) {
            return;
        }

        let active_session = self.current_active_session_view();
        let session_mode = active_session.as_ref().map(|session| session.screen.mode);
        let session_id = self.resolved_terminal_session_id(active_session.as_ref());
        let terminal_input_blocked = self.terminal_input_block_reason().is_some();

        if session_mode.is_some_and(|mode| self.terminal_mouse_capture_active(mode))
            && !terminal_input_blocked
        {
            self.terminal_selection = None;
            self.is_selecting_terminal = false;
            self.last_terminal_mouse_report = None;
            if let Some(cell) = self.grid_position_for_mouse(event.position, window, true) {
                if let Some(sequence) = mouse_button_report(
                    session_mode.unwrap_or_default(),
                    cell,
                    event.button,
                    event.modifiers,
                    true,
                ) {
                    let Some(session_id) = session_id.clone() else {
                        if self.remote_mode.is_some() {
                            self.terminal_notice = Some(
                                "Remote terminal is still starting. Wait a moment and try again."
                                    .to_string(),
                            );
                            cx.notify();
                        }
                        return;
                    };
                    if self.remote_mode.is_some() {
                        self.remote_send_terminal_input(RemoteTerminalInput::Bytes {
                            session_id: session_id.clone(),
                            bytes: sequence.to_vec(),
                        });
                    } else {
                        let _ = self
                            .process_manager
                            .write_bytes_to_session(&session_id, &sequence);
                    }
                    self.last_terminal_mouse_report = Some((cell, Some(event.button)));
                    window.prevent_default();
                }
            }
            return;
        }

        let Some(session) = active_session.as_ref() else {
            self.terminal_selection = None;
            self.is_selecting_terminal = false;
            return;
        };

        if event.button != MouseButton::Left {
            return;
        }

        let Some(endpoint) =
            self.terminal_selection_endpoint_for_mouse(event.position, window, true)
        else {
            self.terminal_selection = None;
            self.is_selecting_terminal = false;
            return;
        };

        if event.modifiers.shift {
            if let Some(selection) = self.terminal_selection.as_mut() {
                selection.head = endpoint;
                selection.moved = selection.anchor != endpoint;
                selection.mode = TerminalSelectionMode::Simple;
            } else {
                self.terminal_selection = Some(TerminalSelection {
                    anchor: endpoint,
                    head: endpoint,
                    moved: false,
                    mode: TerminalSelectionMode::Simple,
                });
            }
            self.is_selecting_terminal = true;
            cx.notify();
            window.prevent_default();
            return;
        }

        match selection_mode_for_click(event.click_count) {
            Some(TerminalSelectionMode::Simple) => {
                self.terminal_selection = Some(TerminalSelection {
                    anchor: endpoint,
                    head: endpoint,
                    moved: false,
                    mode: TerminalSelectionMode::Simple,
                });
                self.is_selecting_terminal = true;
            }
            Some(mode @ (TerminalSelectionMode::Semantic | TerminalSelectionMode::Lines)) => {
                self.terminal_selection =
                    terminal_selection_for_click(&session.screen, endpoint.position, mode);
                self.is_selecting_terminal = false;
                cx.notify();
            }
            None => return,
        }

        window.prevent_default();
    }

    fn handle_terminal_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.handle_terminal_scrollbar_mouse_move(event, window, cx) {
            return;
        }

        let active_session = self.current_active_session_view();
        let session_mode = active_session.as_ref().map(|session| session.screen.mode);
        let terminal_input_blocked = self.terminal_input_block_reason().is_some();
        if session_mode.is_some_and(|mode| self.terminal_mouse_capture_active(mode))
            && !terminal_input_blocked
        {
            if let Some(cell) = self.grid_position_for_mouse(event.position, window, true) {
                let report_key = (cell, event.pressed_button);
                if self.last_terminal_mouse_report != Some(report_key) {
                    if let Some(sequence) = mouse_move_report(
                        session_mode.unwrap_or_default(),
                        cell,
                        event.pressed_button,
                        event.modifiers,
                    ) {
                        let Some(session_id) =
                            self.resolved_terminal_session_id(active_session.as_ref())
                        else {
                            return;
                        };
                        if self.remote_mode.is_some() {
                            self.remote_send_terminal_input(RemoteTerminalInput::Bytes {
                                session_id,
                                bytes: sequence.to_vec(),
                            });
                        } else {
                            let _ = self
                                .process_manager
                                .write_bytes_to_session(&session_id, &sequence);
                        }
                        self.last_terminal_mouse_report = Some(report_key);
                        window.prevent_default();
                    }
                }
            }
            return;
        }

        if !self.is_selecting_terminal || !event.dragging() {
            return;
        }

        let Some(endpoint) =
            self.terminal_selection_endpoint_for_mouse(event.position, window, true)
        else {
            return;
        };

        if let Some(selection) = self.terminal_selection.as_mut() {
            if selection.head != endpoint {
                selection.head = endpoint;
                selection.moved = selection.anchor != endpoint;
                cx.notify();
            }
        }
    }

    fn handle_terminal_scrollbar_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if event.button != MouseButton::Left {
            return false;
        }

        if let Some(session) = self.current_active_session_view() {
            if let Some(geometry) = self.terminal_scrollbar_geometry(window, &session) {
                if self.scrollbar_hit_test(event.position, geometry) {
                    self.terminal_selection = None;
                    self.is_selecting_terminal = false;
                    let grab_offset_px = if self.scrollbar_thumb_contains(event.position, geometry)
                    {
                        let y: f32 = event.position.y.into();
                        (y - geometry.thumb_top).clamp(0.0, geometry.thumb_height)
                    } else {
                        geometry.thumb_height / 2.0
                    };
                    self.terminal_scrollbar_drag = Some(TerminalScrollbarDrag {
                        grab_offset_px,
                        thumb_top_ratio: scrollbar_thumb_top_ratio(
                            session.screen.display_offset,
                            geometry.max_offset,
                        ),
                        last_display_offset: session.screen.display_offset.min(geometry.max_offset),
                    });
                    self.scroll_terminal_from_scrollbar(event.position, geometry, cx);
                    window.prevent_default();
                    return true;
                }
            }
        }

        false
    }

    fn handle_terminal_scrollbar_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(_) = self.terminal_scrollbar_drag else {
            return false;
        };
        if !event.dragging() {
            return false;
        }

        if let Some(session) = self.current_active_session_view() {
            if let Some(geometry) = self.terminal_scrollbar_geometry(window, &session) {
                self.scroll_terminal_from_scrollbar(event.position, geometry, cx);
                window.prevent_default();
                return true;
            }
        }
        self.terminal_scrollbar_drag = None;
        false
    }

    fn handle_terminal_mouse_up(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.handle_terminal_scrollbar_mouse_up(event, window, cx) {
            return;
        }

        let active_session = self.current_active_session_view();
        let session_mode = active_session.as_ref().map(|session| session.screen.mode);
        let terminal_input_blocked = self.terminal_input_block_reason().is_some();
        if session_mode.is_some_and(|mode| self.terminal_mouse_capture_active(mode))
            && !terminal_input_blocked
        {
            if let Some(cell) = self.grid_position_for_mouse(event.position, window, true) {
                if let Some(sequence) = mouse_button_report(
                    session_mode.unwrap_or_default(),
                    cell,
                    event.button,
                    event.modifiers,
                    false,
                ) {
                    let Some(session_id) =
                        self.resolved_terminal_session_id(active_session.as_ref())
                    else {
                        return;
                    };
                    if self.remote_mode.is_some() {
                        self.remote_send_terminal_input(RemoteTerminalInput::Bytes {
                            session_id,
                            bytes: sequence.to_vec(),
                        });
                    } else {
                        let _ = self
                            .process_manager
                            .write_bytes_to_session(&session_id, &sequence);
                    }
                    window.prevent_default();
                }
            }
            self.last_terminal_mouse_report = None;
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        self.finish_terminal_selection(window, cx);
    }

    fn handle_terminal_scrollbar_mouse_up(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> bool {
        if event.button == MouseButton::Left && self.terminal_scrollbar_drag.take().is_some() {
            window.prevent_default();
            return true;
        }
        false
    }

    fn handle_terminal_mouse_up_out(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.handle_terminal_scrollbar_mouse_up(event, window, cx) {
            return;
        }

        let active_session = self.current_active_session_view();
        let session_mode = active_session.as_ref().map(|session| session.screen.mode);
        let terminal_input_blocked = self.terminal_input_block_reason().is_some();
        if session_mode.is_some_and(|mode| self.terminal_mouse_capture_active(mode))
            && !terminal_input_blocked
        {
            let cell = self
                .grid_position_for_mouse(event.position, window, true)
                .unwrap_or(TerminalGridPosition { row: 0, column: 0 });
            if let Some(sequence) = mouse_button_report(
                session_mode.unwrap_or_default(),
                cell,
                event.button,
                event.modifiers,
                false,
            ) {
                let Some(session_id) = self.resolved_terminal_session_id(active_session.as_ref())
                else {
                    return;
                };
                if self.remote_mode.is_some() {
                    self.remote_send_terminal_input(RemoteTerminalInput::Bytes {
                        session_id,
                        bytes: sequence.to_vec(),
                    });
                } else {
                    let _ = self
                        .process_manager
                        .write_bytes_to_session(&session_id, &sequence);
                }
                window.prevent_default();
            }
            self.last_terminal_mouse_report = None;
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        self.finish_terminal_selection(window, cx);
    }

    fn finish_terminal_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selection) = self.terminal_selection {
            if !selection.moved && matches!(selection.mode, TerminalSelectionMode::Simple) {
                self.terminal_selection = None;
                cx.notify();
            }
        }
        if self.state.settings().copy_on_select {
            let _ = self.copy_terminal_selection_to_clipboard(cx);
        }
        self.is_selecting_terminal = false;
        self.terminal_scrollbar_drag = None;
        self.last_terminal_mouse_report = None;
        window.prevent_default();
    }

    fn terminal_mouse_capture_active(
        &self,
        mode: crate::terminal::session::TerminalModeSnapshot,
    ) -> bool {
        mode.mouse_reporting() && !self.state.settings().terminal_mouse_override
    }

    fn handle_terminal_search_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if !self.terminal_search.active {
            return false;
        }

        let key = event.keystroke.key.to_ascii_lowercase();
        match key.as_str() {
            "escape" => {
                self.close_terminal_search_action(cx);
                true
            }
            "backspace" => {
                self.terminal_search.query.pop();
                self.refresh_terminal_search_results(cx);
                true
            }
            "enter" | "down" => {
                self.cycle_terminal_search_match(true, cx);
                true
            }
            "up" => {
                self.cycle_terminal_search_match(false, cx);
                true
            }
            "space" => {
                self.terminal_search.query.push(' ');
                self.refresh_terminal_search_results(cx);
                true
            }
            _ if key.chars().count() == 1
                && !event.keystroke.modifiers.control
                && !event.keystroke.modifiers.platform
                && !event.keystroke.modifiers.alt =>
            {
                self.terminal_search.query.push_str(&event.keystroke.key);
                self.refresh_terminal_search_results(cx);
                true
            }
            _ => false,
        }
    }

    fn handle_terminal_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.handle_wizard_key(event, window, cx) {
            return;
        }
        if self.handle_terminal_search_key(event, cx) {
            window.prevent_default();
            return;
        }
        let active_session = self.current_active_session_view();
        let session_id = self.resolved_terminal_session_id(active_session.as_ref());
        let mode = active_session
            .as_ref()
            .map(|session| session.screen.mode)
            .unwrap_or_default();
        let binding_context = TerminalBindingContext {
            has_selection: active_session
                .as_ref()
                .and_then(|session| self.selection_range(session.screen.cols))
                .is_some(),
            bracketed_paste: mode.bracketed_paste,
        };
        let input_context = TerminalInputContext {
            mode,
            option_as_meta: self.state.settings().option_as_meta,
        };

        let action = translate_key_event(event, binding_context);

        match action {
            TerminalKeyAction::CloseSession => {
                if let Some(tab) = self.state.active_tab().cloned() {
                    self.close_tab_action(&tab.id, cx);
                } else {
                    let Some(session_id) = session_id.as_ref() else {
                        return;
                    };
                    let _ = self.process_manager.close_session(session_id);
                }
                window.prevent_default();
            }
            TerminalKeyAction::CopySelection => {
                if self.copy_terminal_selection_to_clipboard(cx) {
                    window.prevent_default();
                }
            }
            TerminalKeyAction::Paste => {
                if let Some(reason) = self.terminal_input_block_reason() {
                    self.terminal_notice = Some(reason);
                    cx.notify();
                    window.prevent_default();
                    return;
                }
                if self.state.settings().terminal_read_only {
                    self.terminal_notice =
                        Some("Terminal is read-only. Disable it to paste.".to_string());
                    window.prevent_default();
                    return;
                }
                if let Some(clipboard) = cx.read_from_clipboard() {
                    match terminal_clipboard_payload(&clipboard) {
                        Some(TerminalClipboardPayload::Text(text)) => {
                            let Some(session_id) = session_id.clone() else {
                                if self.remote_mode.is_some() {
                                    self.terminal_notice = Some(
                                        "Remote terminal is still starting. Wait a moment and try again."
                                            .to_string(),
                                    );
                                    cx.notify();
                                }
                                window.prevent_default();
                                return;
                            };
                            if self.remote_mode.is_some() {
                                self.remote_send_terminal_input(RemoteTerminalInput::Paste {
                                    session_id: session_id.clone(),
                                    text,
                                });
                            } else {
                                let _ = self.process_manager.paste_to_session(&session_id, &text);
                            }
                        }
                        Some(TerminalClipboardPayload::RawBytes(bytes)) => {
                            let Some(session_id) = session_id.clone() else {
                                if self.remote_mode.is_some() {
                                    self.terminal_notice = Some(
                                        "Remote terminal is still starting. Wait a moment and try again."
                                            .to_string(),
                                    );
                                    cx.notify();
                                }
                                window.prevent_default();
                                return;
                            };
                            if self.remote_mode.is_some() {
                                self.remote_send_terminal_input(RemoteTerminalInput::Bytes {
                                    session_id: session_id.clone(),
                                    bytes,
                                });
                            } else {
                                let _ = self
                                    .process_manager
                                    .write_bytes_to_session(&session_id, &bytes);
                            }
                        }
                        None => {}
                    }
                }
                window.prevent_default();
            }
            TerminalKeyAction::SendInput(input) => {
                if let Some(reason) = self.terminal_input_block_reason() {
                    self.terminal_notice = Some(reason);
                    cx.notify();
                    window.prevent_default();
                    return;
                }
                if self.state.settings().terminal_read_only {
                    self.terminal_notice =
                        Some("Terminal is read-only. Disable it to type.".to_string());
                    window.prevent_default();
                    return;
                }
                if let Some(text) = resolve_terminal_input_text(&input, input_context) {
                    if text == "\u{3}"
                        && matches!(
                            self.state.active_tab().map(|tab| tab.tab_type.clone()),
                            Some(TabType::Server)
                        )
                        && active_session.as_ref().is_some_and(|session| {
                            session.runtime.status.is_live() && !session.runtime.interactive_shell
                        })
                    {
                        if self.remote_mode.is_none() {
                            let Some(session_id) = session_id.as_ref() else {
                                return;
                            };
                            self.process_manager.note_server_interrupt(&session_id);
                        }
                    }
                    let Some(session_id) = session_id.clone() else {
                        if self.remote_mode.is_some() {
                            self.terminal_notice = Some(
                                "Remote terminal is still starting. Wait a moment and try again."
                                    .to_string(),
                            );
                            cx.notify();
                            window.prevent_default();
                        }
                        return;
                    };
                    if self.remote_mode.is_some() {
                        self.remote_send_terminal_input(RemoteTerminalInput::Text {
                            session_id,
                            text,
                        });
                    } else {
                        let _ = self.process_manager.write_to_session(&session_id, &text);
                    }
                    window.prevent_default();
                }
            }
        }
    }

    fn handle_terminal_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(delta_lines) = self.determine_terminal_scroll_lines(event, window) else {
            return;
        };

        if delta_lines == 0 {
            return;
        }

        if let Some(session) = self.current_active_session_view() {
            let Some(session_id) = self.resolved_terminal_session_id(Some(&session)) else {
                return;
            };
            let terminal_input_blocked = self.terminal_input_block_reason().is_some();
            let target_display_offset = session
                .screen
                .display_offset
                .saturating_add_signed(delta_lines as isize)
                .min(session.screen.history_size);
            let selection_endpoint = if self.is_selecting_terminal {
                self.terminal_selection_endpoint_for_mouse_with_display_offset(
                    event.position,
                    window,
                    true,
                    target_display_offset,
                )
            } else {
                None
            };
            if self.terminal_mouse_capture_active(session.screen.mode) && !terminal_input_blocked {
                if let Some(cell) = self.grid_position_for_mouse(event.position, window, true) {
                    if let Some(sequences) =
                        mouse_scroll_report(session.screen.mode, cell, delta_lines, event)
                    {
                        for sequence in sequences {
                            if self.remote_mode.is_some() {
                                self.remote_send_terminal_input(RemoteTerminalInput::Bytes {
                                    session_id: session_id.clone(),
                                    bytes: sequence.to_vec(),
                                });
                            } else {
                                let _ = self
                                    .process_manager
                                    .write_bytes_to_session(&session_id, &sequence);
                            }
                        }
                        self.last_terminal_mouse_report = Some((cell, None));
                    }
                }
            } else if session.screen.mode.alternate_screen
                && session.screen.mode.alternate_scroll
                && !event.modifiers.shift
                && !terminal_input_blocked
            {
                let sequence = alt_scroll_bytes(delta_lines);
                if self.remote_mode.is_some() {
                    self.remote_send_terminal_input(RemoteTerminalInput::Bytes {
                        session_id,
                        bytes: sequence.to_vec(),
                    });
                } else {
                    let _ = self
                        .process_manager
                        .write_bytes_to_session(&session_id, &sequence);
                }
            } else {
                if self.remote_mode.is_some() {
                    self.remote_send_action(RemoteAction::ScrollSession {
                        session_id,
                        delta_lines,
                    });
                } else {
                    let _ = self
                        .process_manager
                        .scroll_session(&session_id, delta_lines);
                }
            }
            if let (Some(selection), Some(endpoint)) =
                (self.terminal_selection.as_mut(), selection_endpoint)
            {
                if selection.head != endpoint {
                    selection.head = endpoint;
                    selection.moved = selection.anchor != endpoint;
                    cx.notify();
                }
            }
        } else {
        }
        window.prevent_default();
    }

    fn grid_position_for_mouse(
        &self,
        position: Point<Pixels>,
        window: &Window,
        clamp_to_terminal: bool,
    ) -> Option<TerminalGridPosition> {
        let session = self.current_active_session_view()?;
        let bounds = self.terminal_text_bounds(window, &session)?;
        terminal_endpoint_for_mouse(position, bounds, clamp_to_terminal)
            .map(|endpoint| endpoint.position)
    }

    fn terminal_selection_endpoint_for_mouse(
        &self,
        position: Point<Pixels>,
        window: &Window,
        clamp_to_terminal: bool,
    ) -> Option<TerminalSelectionEndpoint> {
        let session = self.current_active_session_view()?;
        self.terminal_selection_endpoint_for_mouse_with_display_offset(
            position,
            window,
            clamp_to_terminal,
            session.screen.display_offset,
        )
    }

    fn terminal_selection_endpoint_for_mouse_with_display_offset(
        &self,
        position: Point<Pixels>,
        window: &Window,
        clamp_to_terminal: bool,
        display_offset: usize,
    ) -> Option<TerminalSelectionEndpoint> {
        let session = self.current_active_session_view()?;
        let bounds = self.terminal_text_bounds(window, &session)?;
        let mut endpoint = terminal_endpoint_for_mouse(position, bounds, clamp_to_terminal)?;
        endpoint.position.row =
            buffer_line_for_viewport_row(&session.screen, display_offset, endpoint.position.row);
        Some(endpoint)
    }

    fn determine_terminal_scroll_lines(
        &mut self,
        event: &ScrollWheelEvent,
        window: &Window,
    ) -> Option<i32> {
        let line_height = px(self.terminal_render_metrics(window).line_height);
        match event.touch_phase {
            TouchPhase::Started => {
                self.terminal_scroll_px = px(0.0);
                None
            }
            TouchPhase::Moved => {
                let old_offset = (self.terminal_scroll_px / line_height) as i32;
                self.terminal_scroll_px += event.delta.pixel_delta(line_height).y;
                let new_offset = (self.terminal_scroll_px / line_height) as i32;
                let delta = new_offset - old_offset;
                let viewport_height: f32 = window.viewport_size().height.into();
                self.terminal_scroll_px %=
                    px(viewport_height.max(self.terminal_render_metrics(window).line_height));
                Some(delta)
            }
            _ => {
                let delta = event.delta.pixel_delta(line_height);
                let y: f32 = delta.y.into();
                Some((y / f32::from(line_height)).round() as i32)
            }
        }
    }

    fn terminal_text_bounds(
        &self,
        window: &Window,
        session: &crate::terminal::session::TerminalSessionView,
    ) -> Option<TerminalTextBounds> {
        let mut rows = session.screen.rows.max(1);
        let mut cols = session.screen.cols.max(1);
        let layout = self.terminal_viewport_layout(window, session.runtime.exit.is_some())?;
        let metrics = self.terminal_render_metrics(window);
        let cell_width = metrics.cell_width;
        let row_height = metrics.line_height;
        let scrollbar_width = if self.terminal_has_scrollbar(session) {
            view::TERMINAL_SCROLLBAR_WIDTH_PX
        } else {
            0.0
        };
        let available_width = (layout.available_width - scrollbar_width).max(cell_width);
        let available_height = layout.available_height.max(row_height);
        cols = cols.min((available_width / cell_width).floor().max(1.0) as usize);
        rows = rows.min((available_height / row_height).floor().max(1.0) as usize);
        let width = cols as f32 * cell_width;
        let height = rows as f32 * row_height;

        Some(TerminalTextBounds {
            left: layout.left,
            top: layout.top,
            width,
            height,
            cell_width,
            row_height,
            rows,
            cols,
        })
    }

    fn terminal_viewport_layout(
        &self,
        window: &Window,
        include_exit_banner: bool,
    ) -> Option<TerminalViewportLayout> {
        let viewport = window.viewport_size();
        let viewport_width: f32 = viewport.width.into();
        let viewport_height: f32 = viewport.height.into();
        let left = self.sidebar_width() + 4.0; // px_1() left padding on grid inner
        let mut top = TERMINAL_TOPBAR_HEIGHT_PX;

        if self.startup_notice.is_some() || self.terminal_notice.is_some() {
            top += NOTICE_HEIGHT_PX + STACK_GAP_PX;
        }
        if self
            .current_active_session_view()
            .is_some_and(|session| session.runtime.awaiting_external_editor)
        {
            top += NOTICE_HEIGHT_PX + STACK_GAP_PX;
        }
        if self.terminal_search.active {
            top += SEARCH_BAR_HEIGHT_PX + STACK_GAP_PX;
        }
        if include_exit_banner {
            top += NOTICE_HEIGHT_PX + STACK_GAP_PX;
        }
        top += 2.0; // py(px(2.0)) top on grid inner

        if viewport_width <= left || viewport_height <= top {
            return None;
        }

        let right_padding = 4.0; // px_1() right padding on grid inner
        let bottom_padding = chrome::STATUS_BAR_HEIGHT_PX
            + 2.0  // py(px(2.0)) bottom on grid inner
            + 2.0  // pb(px(2.0)) on body wrapper
            + FOOTER_HEIGHT_PX
            + if self.process_manager.debug_enabled() {
                META_TEXT_HEIGHT_PX + STACK_GAP_PX
            } else {
                0.0
            };

        Some(TerminalViewportLayout {
            left,
            top,
            available_width: (viewport_width - left - right_padding).max(320.0),
            available_height: (viewport_height - top - bottom_padding).max(160.0),
        })
    }

    fn selection_range(&self, screen_cols: usize) -> Option<TerminalSelectionRange> {
        let selection = self.terminal_selection?;
        if !selection.moved {
            return None;
        }

        let (start, end) = ordered_selection(selection.anchor, selection.head);
        let start_column = boundary_column(start, screen_cols);
        let end_column = boundary_column(end, screen_cols);
        if start.position.row == end.position.row && start_column == end_column {
            return None;
        }

        Some(TerminalSelectionRange {
            start_row: start.position.row,
            start_column,
            end_row: end.position.row,
            end_column,
        })
    }

    fn selection_snapshot(
        &self,
        screen: &crate::terminal::session::TerminalScreenSnapshot,
    ) -> Option<view::TerminalSelectionSnapshot> {
        let range = self.selection_range(screen.cols)?;
        let visible_top = top_visible_buffer_line(screen);
        let visible_bottom = visible_top.saturating_add(screen.rows.saturating_sub(1));
        if range.end_row < visible_top || range.start_row > visible_bottom {
            return None;
        }

        let start_row = range.start_row.max(visible_top) - visible_top;
        let end_row = range.end_row.min(visible_bottom) - visible_top;
        let start_column = if range.start_row < visible_top {
            0
        } else {
            range.start_column
        };
        let end_column = if range.end_row > visible_bottom {
            screen.cols
        } else {
            range.end_column
        };
        if start_row == end_row && start_column == end_column {
            return None;
        }

        Some(view::TerminalSelectionSnapshot {
            start_row,
            start_column,
            end_row,
            end_column,
        })
    }

    fn selected_text(&self) -> Option<String> {
        let session = self.current_active_session_view()?;
        let selection = self.selection_range(session.screen.cols)?;
        let session_id = self.resolved_terminal_session_id(Some(&session))?;
        let scrollback = if let Some(remote_mode) = self.remote_mode.as_ref() {
            remote_mode.client.session_scrollback_text(&session_id)?
        } else {
            self.process_manager
                .session_scrollback_text(&session_id)
                .ok()?
        };
        let lines = scrollback.split('\n').collect::<Vec<_>>();
        let mut selected = Vec::new();

        for row in selection.start_row..=selection.end_row {
            let line = lines.get(row).copied().unwrap_or_default();
            let characters: Vec<char> = line.chars().collect();
            let start = if row == selection.start_row {
                selection.start_column.min(characters.len())
            } else {
                0
            };
            let end = if row == selection.end_row {
                selection.end_column.min(characters.len())
            } else {
                characters.len()
            };
            let mut segment: String = characters[start..end].iter().collect();
            while segment.ends_with(' ') {
                segment.pop();
            }
            selected.push(segment);
        }

        Some(selected.join("\n"))
    }

    fn copy_terminal_selection_to_clipboard(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.selected_text() else {
            return false;
        };

        cx.write_to_clipboard(ClipboardItem::new_string(text));
        if !self.state.settings().keep_selection_on_copy {
            self.terminal_selection = None;
            cx.notify();
        }
        true
    }

    fn sync_window_title(&mut self, window: &mut Window, runtime: &crate::state::RuntimeState) {
        let next_title = current_window_title(&self.state, runtime);
        if self.last_window_title.as_deref() == Some(next_title.as_str()) {
            return;
        }
        window.set_window_title(&next_title);
        self.last_window_title = Some(next_title);
    }

    fn capture_window_bounds(&mut self, window: &mut Window) {
        let wb = window.window_bounds();
        let bounds = wb.get_bounds();
        let maximized = matches!(wb, WindowBounds::Maximized(_));
        self.state.window_bounds = Some(crate::models::WindowBoundsState {
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
            maximized,
        });
    }
}

impl Render for NativeShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let render_started = Instant::now();
        self.capture_window_bounds(window);
        self.sync_remote_host_config_from_service();
        if self.remote_mode.is_some() {
            self.sync_remote_session_subscriptions();
        }
        if let Some(display_offset) = self.pending_terminal_display_offset.take() {
            let active_session = self.current_active_session_view();
            if let Some(session_id) = self.resolved_terminal_session_id(active_session.as_ref()) {
                if self.remote_mode.is_some() {
                    self.remote_send_action(RemoteAction::ScrollSessionToOffset {
                        session_id,
                        display_offset,
                    });
                } else {
                    let _ = self
                        .process_manager
                        .scroll_session_to_offset(&session_id, display_offset);
                }
            }
        }

        if self.remote_mode.is_none() {
            let _ = self.pump_remote_host_requests(cx);
        }

        let local_runtime_snapshot = self.process_manager.runtime_state();
        if self.remote_mode.is_none() {
            self.sync_server_port_snapshot(&local_runtime_snapshot, cx);
            self.sync_remote_host_snapshot_if_due(&local_runtime_snapshot);
        }

        let runtime_snapshot = self.current_runtime_snapshot();
        self.sync_window_title(window, &runtime_snapshot);
        let server_indicators = derive_server_indicator_states(
            &self.state,
            &runtime_snapshot,
            &self.current_port_statuses(),
        );
        let updater_snapshot = self.updater.snapshot();
        let remote_status_bar = self.remote_status_bar_state();
        self.sync_settings_remote_draft();
        let allow_editor_mutation = self.remote_mode.is_none() || self.remote_has_control();
        let editor_model = self.editor_panel.clone().map(|panel| EditorPaneModel {
            allow_mutation: allow_editor_mutation
                || matches!(panel, EditorPanel::Settings(_) | EditorPanel::UiPreview(_)),
            panel,
            active_field: self.editor_active_field,
            cursor: self.editor_cursor,
            selection_anchor: self.editor_selection_anchor,
            notice: self.editor_notice.clone(),
            updater: updater_snapshot.clone(),
        });
        let terminal_model = if editor_model.is_none() {
            Some(self.sync_terminal_session(window, cx))
        } else {
            None
        };

        let make_open_settings_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.open_settings_action(cx);
                }))
            };
        let make_open_remote_handler = || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
            Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.ensure_remote_settings_open(cx);
            }))
        };
        let make_remote_status_action_handler = |action: RemoteStatusBarAction| -> Box<
            dyn Fn(&MouseDownEvent, &mut Window, &mut App),
        > {
            Box::new(
                cx.listener(move |this, _: &MouseDownEvent, _window, cx| match action {
                    RemoteStatusBarAction::ConnectPreferred => {
                        this.connect_preferred_remote_host_action(cx);
                    }
                    RemoteStatusBarAction::RetryReconnect => {
                        this.force_remote_reconnect_now(cx);
                    }
                    RemoteStatusBarAction::DisconnectRemote => {
                        this.disconnect_remote_host(Some(
                            "Disconnected from remote host.".to_string(),
                        ));
                        cx.notify();
                    }
                    RemoteStatusBarAction::TakeRemoteControl => {
                        if let Some(remote_mode) = this.remote_mode.as_ref() {
                            remote_mode.client.take_control();
                        }
                        this.editor_notice =
                            Some("This client now controls the remote host.".to_string());
                        this.sync_settings_remote_draft();
                        cx.notify();
                    }
                    RemoteStatusBarAction::ReleaseRemoteControl => {
                        if let Some(remote_mode) = this.remote_mode.as_ref() {
                            remote_mode.client.release_control();
                        }
                        this.editor_notice =
                            Some("This client released control and is now a viewer.".to_string());
                        this.sync_settings_remote_draft();
                        cx.notify();
                    }
                    RemoteStatusBarAction::TakeHostControl => {
                        this.remote_host_service.take_local_control();
                        this.editor_notice =
                            Some("This machine controls the host again.".to_string());
                        this.sync_settings_remote_draft();
                        cx.notify();
                    }
                    RemoteStatusBarAction::CopyPairToken => {
                        this.copy_remote_pairing_token_action(cx);
                    }
                    RemoteStatusBarAction::OpenRemoteSettings => {
                        this.ensure_remote_settings_open(cx);
                    }
                }),
            )
        };
        let remote_primary_handler = remote_status_bar
            .primary_action
            .map(|action| move || make_remote_status_action_handler(action));
        let remote_secondary_handler = remote_status_bar
            .secondary_action
            .map(|action| move || make_remote_status_action_handler(action));
        let remote_tertiary_handler = remote_status_bar
            .tertiary_action
            .map(|action| move || make_remote_status_action_handler(action));
        let make_open_git_handler = || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
            Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.open_git_window(cx);
            }))
        };
        let make_toggle_sidebar_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.toggle_sidebar_action(cx);
                }))
            };
        let make_stop_all_servers_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.stop_all_servers_action(cx);
                }))
            };
        let make_add_project_handler = || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
            Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.open_add_project_action(cx);
            }))
        };
        let make_edit_project_handler =
            |project_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.sidebar_context_menu = None;
                    this.open_edit_project_action(&project_id, cx);
                }))
            };
        let make_project_notes_handler =
            |project_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.sidebar_context_menu = None;
                    this.open_project_notes_action(&project_id, cx);
                }))
            };
        let make_delete_project_handler =
            |project_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.sidebar_context_menu = None;
                    this.delete_project_action(&project_id, cx);
                }))
            };
        let make_toggle_project_collapse_handler =
            |project_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.state.toggle_project_collapsed(&project_id);
                    this.save_session_state();
                    cx.notify();
                }))
            };
        let make_move_project_up_handler =
            |project_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.state.move_project(&project_id, -1);
                    this.save_config_state();
                    cx.notify();
                }))
            };
        let make_move_project_down_handler =
            |project_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.state.move_project(&project_id, 1);
                    this.save_config_state();
                    cx.notify();
                }))
            };
        let make_add_folder_handler =
            |project_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.sidebar_context_menu = None;
                    this.open_add_folder_action(&project_id, cx);
                }))
            };
        let make_edit_folder_handler =
            |project_id: String,
             folder_id: String|
             -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.sidebar_context_menu = None;
                    this.open_edit_folder_action(&project_id, &folder_id, cx);
                }))
            };
        let make_delete_folder_handler =
            |project_id: String,
             folder_id: String|
             -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.sidebar_context_menu = None;
                    this.delete_folder_action(&project_id, &folder_id, cx);
                }))
            };
        let make_add_command_handler =
            |project_id: String,
             folder_id: String|
             -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.sidebar_context_menu = None;
                    this.open_add_command_action(&project_id, &folder_id, cx);
                }))
            };
        let make_edit_command_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.sidebar_context_menu = None;
                    this.open_edit_command_action(&command_id, cx);
                }))
            };
        let make_delete_command_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.sidebar_context_menu = None;
                    this.delete_command_action(&command_id, cx);
                }))
            };
        let make_add_ssh_handler = || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
            Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.sidebar_context_menu = None;
                this.open_add_ssh_action(cx);
            }))
        };
        let make_edit_ssh_handler =
            |connection_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.sidebar_context_menu = None;
                    this.open_edit_ssh_action(&connection_id, cx);
                }))
            };
        let make_delete_ssh_handler =
            |connection_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.sidebar_context_menu = None;
                    this.delete_ssh_action(&connection_id, cx);
                }))
            };
        let make_open_ssh_handler =
            |connection_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.open_ssh_tab_action(&connection_id, cx);
                }))
            };
        let make_connect_ssh_handler =
            |connection_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.sidebar_context_menu = None;
                    this.connect_ssh_action(&connection_id, window, cx);
                }))
            };
        let make_disconnect_ssh_handler =
            |connection_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.sidebar_context_menu = None;
                    this.disconnect_ssh_action(&connection_id, cx);
                }))
            };
        let make_restart_ssh_handler =
            |connection_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.sidebar_context_menu = None;
                    this.restart_ssh_action(&connection_id, window, cx);
                }))
            };
        let make_respond_to_ssh_prompt_handler =
            |session_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.respond_to_ssh_prompt_action(&session_id, cx);
                }))
            };
        let make_toggle_terminal_search_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.toggle_terminal_search_action(cx);
                }))
            };
        let make_close_terminal_search_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.close_terminal_search_action(cx);
                }))
            };
        let make_search_prev_handler = || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
            Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.cycle_terminal_search_match(false, cx);
            }))
        };
        let make_search_next_handler = || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
            Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.cycle_terminal_search_match(true, cx);
            }))
        };
        let make_search_case_handler = || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
            Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.toggle_terminal_search_case_action(cx);
            }))
        };
        let make_prev_prompt_handler = || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
            Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.jump_terminal_prompt(true, cx);
            }))
        };
        let make_next_prompt_handler = || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
            Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.jump_terminal_prompt(false, cx);
            }))
        };
        let make_export_screen_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.export_terminal_view_action(false, false, cx);
                }))
            };
        let make_export_scrollback_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.export_terminal_view_action(true, false, cx);
                }))
            };
        let make_export_selection_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.export_terminal_view_action(false, true, cx);
                }))
            };
        let make_toggle_mouse_override_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.toggle_terminal_mouse_override_action(cx);
                }))
            };
        let make_toggle_read_only_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.toggle_terminal_read_only_action(cx);
                }))
            };
        let make_start_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.sidebar_context_menu = None;
                    this.start_server_action(&command_id, false, window, cx);
                }))
            };
        let make_focused_start_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.start_server_action(&command_id, true, window, cx);
                }))
            };
        let make_stop_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.sidebar_context_menu = None;
                    this.stop_server_action(&command_id, cx);
                }))
            };
        let make_restart_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.sidebar_context_menu = None;
                    this.restart_server_action(&command_id, window, cx);
                }))
            };
        let make_clear_output_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.clear_server_output_action(&command_id, cx);
                }))
            };
        let make_open_server_url_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.open_server_url_action(&command_id, cx);
                }))
            };
        let make_kill_port_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.kill_server_port_action(&command_id, window, cx);
                }))
            };
        let make_select_server_handler =
            |command_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.select_server_tab_action(&command_id, cx);
                }))
            };
        let make_launch_claude_handler =
            |project_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.launch_ai_action(&project_id, TabType::Claude, window, cx);
                }))
            };
        let make_launch_codex_handler =
            |project_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.launch_ai_action(&project_id, TabType::Codex, window, cx);
                }))
            };
        let make_select_ai_handler =
            |tab_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.select_ai_tab_action(&tab_id, window, cx);
                }))
            };
        let make_restart_ai_handler =
            |tab_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.restart_ai_tab_action(&tab_id, window, cx);
                }))
            };
        let make_close_ai_handler =
            |tab_id: String| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.close_ai_tab_action(&tab_id, cx);
                }))
            };
        let make_toggle_context_menu_handler = |menu: sidebar::SidebarContextMenu| -> Box<
            dyn Fn(&MouseDownEvent, &mut Window, &mut App),
        > {
            Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if this.sidebar_context_menu.as_ref() == Some(&menu) {
                    this.sidebar_context_menu = None;
                } else {
                    this.sidebar_context_menu = Some(menu.clone());
                }
                cx.notify();
            }))
        };
        let make_dismiss_context_menu_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    if this.sidebar_context_menu.is_some() {
                        this.sidebar_context_menu = None;
                        cx.notify();
                    }
                }))
            };
        let make_wizard_action_handler = |action: workspace::WizardAction| -> Box<
            dyn Fn(&MouseDownEvent, &mut Window, &mut App),
        > {
            Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                match &action {
                    workspace::WizardAction::Cancel => {
                        this.add_project_wizard = None;
                        cx.notify();
                    }
                    workspace::WizardAction::Create => {
                        this.wizard_create_action(cx);
                    }
                    workspace::WizardAction::SelectColor(color) => {
                        if let Some(wizard) = this.add_project_wizard.as_mut() {
                            wizard.color = color.clone();
                            cx.notify();
                        }
                    }
                    workspace::WizardAction::PickRootFolder => {
                        this.wizard_pick_root_folder(cx);
                    }
                    workspace::WizardAction::ToggleFolder(path) => {
                        if let Some(wizard) = this.add_project_wizard.as_mut() {
                            if !wizard.selected_folders.insert(path.clone()) {
                                wizard.selected_folders.remove(path);
                            }
                            cx.notify();
                        }
                    }
                    workspace::WizardAction::ClickName => {
                        if let Some(wizard) = this.add_project_wizard.as_mut() {
                            wizard.name_focused = true;
                            wizard.cursor = wizard.name.len();
                            cx.notify();
                        }
                    }
                    workspace::WizardAction::Configure => {
                        if let Some(wizard) = this.add_project_wizard.as_mut() {
                            // Populate defaults for step 2
                            for entry in &wizard.scan_entries {
                                if wizard.selected_folders.contains(&entry.path) {
                                    wizard
                                        .selected_scripts
                                        .entry(entry.path.clone())
                                        .or_insert_with(|| {
                                            scanner_service::auto_selected_script_names(
                                                &entry.scripts,
                                            )
                                            .into_iter()
                                            .collect()
                                        });
                                    wizard
                                        .selected_port_variables
                                        .entry(entry.path.clone())
                                        .or_insert_with(|| {
                                            scanner_service::auto_selected_port_variable(
                                                &entry.ports,
                                            )
                                        });
                                }
                            }
                            // Prune deselected folders
                            wizard
                                .selected_scripts
                                .retain(|p, _| wizard.selected_folders.contains(p));
                            wizard
                                .selected_port_variables
                                .retain(|p, _| wizard.selected_folders.contains(p));
                            wizard.step = 2;
                            cx.notify();
                        }
                    }
                    workspace::WizardAction::Back => {
                        if let Some(wizard) = this.add_project_wizard.as_mut() {
                            wizard.step = 1;
                            cx.notify();
                        }
                    }
                    workspace::WizardAction::ToggleScript {
                        folder_path,
                        script_name,
                    } => {
                        if let Some(wizard) = this.add_project_wizard.as_mut() {
                            let scripts = wizard
                                .selected_scripts
                                .entry(folder_path.clone())
                                .or_default();
                            if !scripts.insert(script_name.clone()) {
                                scripts.remove(script_name);
                            }
                            cx.notify();
                        }
                    }
                    workspace::WizardAction::SelectPortVariable {
                        folder_path,
                        variable,
                    } => {
                        if let Some(wizard) = this.add_project_wizard.as_mut() {
                            wizard
                                .selected_port_variables
                                .insert(folder_path.clone(), variable.clone());
                            cx.notify();
                        }
                    }
                }
            }))
        };
        let make_install_update_handler =
            || -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                Box::new(cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.install_update_action(cx);
                }))
            };
        let editor_entity = cx.weak_entity();
        let make_editor_action_handler = {
            let editor_entity = editor_entity.clone();
            Arc::new(
                move |action: EditorAction| -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                    let editor_entity = editor_entity.clone();
                    Box::new(move |_, window, app| {
                        let _ = editor_entity.update(app, |this, cx| {
                            this.apply_editor_action(action.clone(), window, cx);
                        });
                    })
                },
            )
        };
        let make_editor_focus_handler = {
            let editor_entity = editor_entity.clone();
            Arc::new(
                move |field: EditorField,
                      cursor: usize|
                      -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
                    let editor_entity = editor_entity.clone();
                    Box::new(move |event, window, app| {
                        let shift = event.modifiers.shift;
                        let _ = editor_entity.update(app, |this, cx| {
                            this.focus_editor_field_at(field, cursor, shift, window, cx);
                        });
                    })
                },
            )
        };
        let make_editor_drag_handler = {
            let editor_entity = editor_entity.clone();
            Arc::new(
                move |field: EditorField,
                      cursor: usize|
                      -> Box<dyn Fn(&MouseMoveEvent, &mut Window, &mut App)> {
                    let editor_entity = editor_entity.clone();
                    Box::new(move |_, _window, app| {
                        let _ = editor_entity.update(app, |this, cx| {
                            this.drag_editor_field_to(field, cursor, cx);
                        });
                    })
                },
            )
        };

        if let Some(model) = terminal_model.as_ref() {
            if model
                .session
                .as_ref()
                .map(|session| {
                    session.runtime.status.is_live() || session.runtime.interactive_shell
                })
                .unwrap_or(false)
            {
                window.request_animation_frame();
            }

            let active_session_id = self.state.active_terminal_spec().session_id;
            self.process_manager
                .record_frame(&active_session_id, render_started.elapsed());
        }
        if updater_snapshot.is_busy() {
            window.request_animation_frame();
        }
        if self.terminal_scrollbar_drag.is_some() || self.pending_terminal_display_offset.is_some()
        {
            window.request_animation_frame();
        }
        if runtime_snapshot
            .sessions
            .values()
            .any(|s| matches!(s.ai_activity, Some(crate::state::AiActivity::Thinking)))
        {
            window.request_animation_frame();
        }

        let terminal_actions = terminal_model.as_ref().map(|model| {
            let controls = model.runtime_controls.as_ref();
            let command_id = self.state.active_terminal_spec().session_id;
            view::TerminalPaneActions {
                on_start_server: controls
                    .filter(|controls| controls.can_start)
                    .map(|_| make_focused_start_handler(command_id.clone())),
                on_stop_server: controls
                    .filter(|controls| controls.can_stop)
                    .map(|_| make_stop_handler(command_id.clone())),
                on_restart_server: controls
                    .filter(|controls| controls.can_restart)
                    .map(|_| make_restart_handler(command_id.clone())),
                on_clear_output: controls
                    .filter(|controls| controls.can_clear)
                    .map(|_| make_clear_output_handler(command_id.clone())),
                on_kill_port: controls
                    .filter(|controls| controls.can_kill_port)
                    .map(|_| make_kill_port_handler(command_id.clone())),
                on_open_local_url: controls
                    .filter(|controls| controls.can_open_url)
                    .map(|_| make_open_server_url_handler(command_id.clone())),
                on_prompt_action: controls
                    .and_then(|controls| controls.prompt_action_label.as_ref())
                    .map(|_| make_respond_to_ssh_prompt_handler(command_id.clone())),
                on_toggle_search: controls
                    .filter(|controls| controls.can_search)
                    .map(|_| make_toggle_terminal_search_handler()),
                on_search_prev: controls
                    .filter(|controls| controls.search_active)
                    .map(|_| make_search_prev_handler()),
                on_search_next: controls
                    .filter(|controls| controls.search_active)
                    .map(|_| make_search_next_handler()),
                on_toggle_search_case: controls
                    .filter(|controls| controls.search_active)
                    .map(|_| make_search_case_handler()),
                on_close_search: controls
                    .filter(|controls| controls.search_active)
                    .map(|_| make_close_terminal_search_handler()),
                on_jump_prev_prompt: controls
                    .filter(|controls| controls.can_jump_prev_prompt)
                    .map(|_| make_prev_prompt_handler()),
                on_jump_next_prompt: controls
                    .filter(|controls| controls.can_jump_next_prompt)
                    .map(|_| make_next_prompt_handler()),
                on_export_screen: controls
                    .filter(|controls| controls.can_export_screen)
                    .map(|_| make_export_screen_handler()),
                on_export_scrollback: controls
                    .filter(|controls| controls.can_export_scrollback)
                    .map(|_| make_export_scrollback_handler()),
                on_export_selection: controls
                    .filter(|controls| controls.can_export_selection)
                    .map(|_| make_export_selection_handler()),
                on_toggle_mouse_override: Some(make_toggle_mouse_override_handler()),
                on_toggle_read_only: Some(make_toggle_read_only_handler()),
                scrollbar: Some(view::TerminalScrollbarActions {
                    on_mouse_down: Arc::new(cx.listener(
                        |this, event: &MouseDownEvent, window, cx| {
                            this.handle_terminal_scrollbar_mouse_down(event, window, cx);
                        },
                    )),
                    on_mouse_move: Arc::new(cx.listener(
                        |this, event: &MouseMoveEvent, window, cx| {
                            this.handle_terminal_scrollbar_mouse_move(event, window, cx);
                        },
                    )),
                    on_mouse_up: Arc::new(cx.listener(|this, event: &MouseUpEvent, window, cx| {
                        this.handle_terminal_scrollbar_mouse_up(event, window, cx);
                    })),
                }),
            }
        });

        div()
            .size_full()
            .flex()
            .bg(rgb(theme::APP_BG))
            .text_color(rgb(theme::TEXT_PRIMARY))
            .child(sidebar::render_sidebar(
                &self.state,
                &runtime_snapshot,
                &server_indicators,
                sidebar::SidebarActions {
                    mutations_allowed: self.remote_mode.is_none() || self.remote_has_control(),
                    on_open_settings: &make_open_settings_handler,
                    on_toggle_sidebar: &make_toggle_sidebar_handler,
                    on_stop_all_servers: &make_stop_all_servers_handler,
                    on_add_project: &make_add_project_handler,
                    on_edit_project: &make_edit_project_handler,
                    on_open_project_notes: &make_project_notes_handler,
                    on_delete_project: &make_delete_project_handler,
                    on_add_folder: &make_add_folder_handler,
                    on_edit_folder: &make_edit_folder_handler,
                    on_delete_folder: &make_delete_folder_handler,
                    on_add_command: &make_add_command_handler,
                    on_edit_command: &make_edit_command_handler,
                    on_delete_command: &make_delete_command_handler,
                    on_add_ssh: &make_add_ssh_handler,
                    on_edit_ssh: &make_edit_ssh_handler,
                    on_delete_ssh: &make_delete_ssh_handler,
                    on_open_ssh_tab: &make_open_ssh_handler,
                    on_connect_ssh: &make_connect_ssh_handler,
                    on_disconnect_ssh: &make_disconnect_ssh_handler,
                    on_restart_ssh: &make_restart_ssh_handler,
                    on_start_server: &make_start_handler,
                    on_stop_server: &make_stop_handler,
                    on_restart_server: &make_restart_handler,
                    on_select_server_tab: &make_select_server_handler,
                    on_launch_claude: &make_launch_claude_handler,
                    on_launch_codex: &make_launch_codex_handler,
                    on_select_ai_tab: &make_select_ai_handler,
                    on_restart_ai_tab: &make_restart_ai_handler,
                    on_close_ai_tab: &make_close_ai_handler,
                    on_toggle_project_collapse: &make_toggle_project_collapse_handler,
                    on_move_project_up: &make_move_project_up_handler,
                    on_move_project_down: &make_move_project_down_handler,
                    on_toggle_context_menu: &make_toggle_context_menu_handler,
                    on_dismiss_context_menu: &make_dismiss_context_menu_handler,
                    open_context_menu: &self.sidebar_context_menu,
                    on_open_git: &make_open_git_handler,
                },
            ))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(if let Some(model) = editor_model.as_ref() {
                        if self.editor_needs_focus {
                            self.focus_editor(window);
                        }

                        div()
                            .flex_1()
                            .overflow_hidden()
                            .track_focus(&self.editor_focus)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(Self::handle_editor_mouse_down),
                            )
                            .on_key_down(cx.listener(Self::handle_editor_key))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(Self::handle_editor_mouse_up),
                            )
                            .on_mouse_up_out(
                                MouseButton::Left,
                                cx.listener(Self::handle_editor_mouse_up),
                            )
                            .child(workspace::render_editor_surface(
                                model,
                                workspace::EditorActions {
                                    on_action: make_editor_action_handler.clone(),
                                    on_focus_at: make_editor_focus_handler.clone(),
                                    on_drag_to: make_editor_drag_handler.clone(),
                                },
                            ))
                    } else {
                        let model = terminal_model.as_ref().expect("terminal model");
                        div()
                            .flex_1()
                            .track_focus(&self.terminal_focus)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(Self::handle_terminal_mouse_down),
                            )
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(Self::handle_terminal_mouse_down),
                            )
                            .on_mouse_down(
                                MouseButton::Middle,
                                cx.listener(Self::handle_terminal_mouse_down),
                            )
                            .on_mouse_move(cx.listener(Self::handle_terminal_mouse_move))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(Self::handle_terminal_mouse_up),
                            )
                            .on_mouse_up(
                                MouseButton::Right,
                                cx.listener(Self::handle_terminal_mouse_up),
                            )
                            .on_mouse_up(
                                MouseButton::Middle,
                                cx.listener(Self::handle_terminal_mouse_up),
                            )
                            .on_mouse_up_out(
                                MouseButton::Left,
                                cx.listener(Self::handle_terminal_mouse_up_out),
                            )
                            .on_mouse_up_out(
                                MouseButton::Right,
                                cx.listener(Self::handle_terminal_mouse_up_out),
                            )
                            .on_mouse_up_out(
                                MouseButton::Middle,
                                cx.listener(Self::handle_terminal_mouse_up_out),
                            )
                            .on_key_down(cx.listener(Self::handle_terminal_key))
                            .on_scroll_wheel(cx.listener(Self::handle_terminal_scroll))
                            .child(view::render_terminal_surface(model, terminal_actions))
                    })
                    .child(chrome::render_status_bar(
                        &runtime_snapshot,
                        &updater_snapshot,
                        Some(&remote_status_bar.model),
                        chrome::StatusBarActions {
                            on_install_update: &make_install_update_handler,
                            on_open_remote: &make_open_remote_handler,
                            on_remote_primary: remote_primary_handler.as_ref().map(|handler| {
                                handler
                                    as &dyn Fn() -> Box<
                                        dyn Fn(&MouseDownEvent, &mut Window, &mut App),
                                    >
                            }),
                            on_remote_secondary: remote_secondary_handler.as_ref().map(|handler| {
                                handler
                                    as &dyn Fn() -> Box<
                                        dyn Fn(&MouseDownEvent, &mut Window, &mut App),
                                    >
                            }),
                            on_remote_tertiary: remote_tertiary_handler.as_ref().map(|handler| {
                                handler
                                    as &dyn Fn() -> Box<
                                        dyn Fn(&MouseDownEvent, &mut Window, &mut App),
                                    >
                            }),
                        },
                    )),
            )
            .children(self.add_project_wizard.as_ref().map(|wizard| {
                workspace::render_add_project_wizard(
                    wizard,
                    workspace::WizardActions {
                        on_action: &make_wizard_action_handler,
                    },
                )
                .into_any_element()
            }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalKeyAction {
    SendInput(TerminalInputAction),
    Paste,
    CopySelection,
    CloseSession,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalInputAction {
    SendText(String),
    SendKeystroke(Keystroke),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalClipboardPayload {
    Text(String),
    RawBytes(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct TerminalBindingContext {
    has_selection: bool,
    bracketed_paste: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalInputContext {
    mode: crate::terminal::session::TerminalModeSnapshot,
    option_as_meta: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalBindingSpec {
    key: &'static str,
    shortcut: TerminalShortcut,
    selection: Option<bool>,
    bracketed_paste: Option<bool>,
    action: TerminalBindingOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalShortcut {
    Secondary { shift: bool },
    Control { shift: bool },
    ShiftOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalBindingOutcome {
    CloseSession,
    CopySelection,
    Paste,
    SendText(&'static str),
}

fn ordered_selection(
    anchor: TerminalSelectionEndpoint,
    head: TerminalSelectionEndpoint,
) -> (TerminalSelectionEndpoint, TerminalSelectionEndpoint) {
    if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    }
}

#[derive(Debug, Clone, Copy)]
enum TerminalMouseFormat {
    Sgr,
    Normal { utf8: bool },
}

impl TerminalMouseFormat {
    fn from_mode(mode: crate::terminal::session::TerminalModeSnapshot) -> Self {
        if mode.sgr_mouse {
            Self::Sgr
        } else {
            Self::Normal {
                utf8: mode.utf8_mouse,
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum TerminalMouseButton {
    LeftButton = 0,
    MiddleButton = 1,
    RightButton = 2,
    LeftMove = 32,
    MiddleMove = 33,
    RightMove = 34,
    NoneMove = 35,
    ScrollUp = 64,
    ScrollDown = 65,
}

impl TerminalMouseButton {
    fn from_button(button: MouseButton) -> Option<Self> {
        match button {
            MouseButton::Left => Some(Self::LeftButton),
            MouseButton::Right => Some(Self::MiddleButton),
            MouseButton::Middle => Some(Self::RightButton),
            MouseButton::Navigate(_) => None,
        }
    }

    fn from_move_button(button: Option<MouseButton>) -> Option<Self> {
        match button {
            Some(MouseButton::Left) => Some(Self::LeftMove),
            Some(MouseButton::Middle) => Some(Self::MiddleMove),
            Some(MouseButton::Right) => Some(Self::RightMove),
            Some(MouseButton::Navigate(_)) => None,
            None => Some(Self::NoneMove),
        }
    }

    fn from_scroll(event: &ScrollWheelEvent) -> Self {
        let positive = match event.delta {
            gpui::ScrollDelta::Pixels(delta) => delta.y > px(0.0),
            gpui::ScrollDelta::Lines(delta) => delta.y > 0.0,
        };
        if positive {
            Self::ScrollUp
        } else {
            Self::ScrollDown
        }
    }
}

fn selection_mode_for_click(click_count: usize) -> Option<TerminalSelectionMode> {
    match click_count {
        0 => None,
        1 => Some(TerminalSelectionMode::Simple),
        2 => Some(TerminalSelectionMode::Semantic),
        _ => Some(TerminalSelectionMode::Lines),
    }
}

fn boundary_column(endpoint: TerminalSelectionEndpoint, screen_cols: usize) -> usize {
    match endpoint.side {
        TerminalCellSide::Left => endpoint.position.column.min(screen_cols),
        TerminalCellSide::Right => (endpoint.position.column + 1).min(screen_cols),
    }
}

fn endpoint_at_boundary(
    row: usize,
    boundary: usize,
    screen_cols: usize,
) -> TerminalSelectionEndpoint {
    if screen_cols == 0 || boundary == 0 {
        return TerminalSelectionEndpoint {
            position: TerminalGridPosition { row, column: 0 },
            side: TerminalCellSide::Left,
        };
    }

    TerminalSelectionEndpoint {
        position: TerminalGridPosition {
            row,
            column: boundary
                .saturating_sub(1)
                .min(screen_cols.saturating_sub(1)),
        },
        side: TerminalCellSide::Right,
    }
}

fn terminal_selection_for_click(
    screen: &crate::terminal::session::TerminalScreenSnapshot,
    position: TerminalGridPosition,
    mode: TerminalSelectionMode,
) -> Option<TerminalSelection> {
    let visible_top = top_visible_buffer_line(screen);
    let viewport_row = position
        .row
        .saturating_sub(visible_top)
        .min(screen.lines.len().saturating_sub(1));
    match mode {
        TerminalSelectionMode::Simple => Some(TerminalSelection {
            anchor: TerminalSelectionEndpoint {
                position,
                side: TerminalCellSide::Left,
            },
            head: TerminalSelectionEndpoint {
                position,
                side: TerminalCellSide::Left,
            },
            moved: false,
            mode,
        }),
        TerminalSelectionMode::Semantic => {
            let line = screen.lines.get(viewport_row)?;
            let (start, end) = semantic_selection_bounds(line, position.column, screen.cols);
            Some(TerminalSelection {
                anchor: endpoint_at_boundary(position.row, start, screen.cols),
                head: endpoint_at_boundary(position.row, end, screen.cols),
                moved: start != end,
                mode,
            })
        }
        TerminalSelectionMode::Lines => Some(TerminalSelection {
            anchor: endpoint_at_boundary(position.row, 0, screen.cols),
            head: endpoint_at_boundary(position.row, screen.cols, screen.cols),
            moved: screen.cols > 0,
            mode,
        }),
    }
}

fn top_visible_buffer_line(screen: &crate::terminal::session::TerminalScreenSnapshot) -> usize {
    screen
        .total_lines
        .saturating_sub(screen.rows.max(1))
        .saturating_sub(screen.display_offset)
}

fn buffer_line_for_viewport_row(
    screen: &crate::terminal::session::TerminalScreenSnapshot,
    display_offset: usize,
    viewport_row: usize,
) -> usize {
    let top = screen
        .total_lines
        .saturating_sub(screen.rows.max(1))
        .saturating_sub(display_offset);
    top.saturating_add(viewport_row.min(screen.rows.saturating_sub(1)))
        .min(screen.total_lines.saturating_sub(1))
}

fn semantic_selection_bounds(
    line: &[crate::terminal::session::TerminalCellSnapshot],
    column: usize,
    screen_cols: usize,
) -> (usize, usize) {
    let len = line.len().min(screen_cols);
    if len == 0 {
        return (0, 0);
    }

    let column = column.min(len.saturating_sub(1));
    let whitespace = line[column].character.is_whitespace();
    let mut start = column;
    while start > 0 && line[start - 1].character.is_whitespace() == whitespace {
        start -= 1;
    }

    let mut end = column + 1;
    while end < len && line[end].character.is_whitespace() == whitespace {
        end += 1;
    }

    (start, end)
}

fn terminal_endpoint_for_mouse(
    position: Point<Pixels>,
    bounds: TerminalTextBounds,
    clamp_to_terminal: bool,
) -> Option<TerminalSelectionEndpoint> {
    if bounds.cols == 0 || bounds.rows == 0 {
        return None;
    }

    let left = bounds.left;
    let top = bounds.top;
    let right = bounds.left + bounds.width;
    let bottom = bounds.top + bounds.height;
    let mut x: f32 = position.x.into();
    let mut y: f32 = position.y.into();

    if !clamp_to_terminal && (x < left || y < top || x >= right || y >= bottom) {
        return None;
    }

    if clamp_to_terminal {
        x = x.clamp(left, right);
        y = y.clamp(top, bottom);
    }

    let relative_x = (x - left).max(0.0);
    let relative_y = (y - top).max(0.0);
    let mut column = (relative_x / bounds.cell_width).floor() as usize;
    let mut row = (relative_y / bounds.row_height).floor() as usize;
    let mut side = if relative_x % bounds.cell_width > bounds.cell_width / 2.0 {
        TerminalCellSide::Right
    } else {
        TerminalCellSide::Left
    };

    if relative_x >= bounds.width {
        column = bounds.cols.saturating_sub(1);
        side = TerminalCellSide::Right;
    } else {
        column = column.min(bounds.cols.saturating_sub(1));
    }

    if y < top {
        row = 0;
        side = TerminalCellSide::Left;
    } else if relative_y >= bounds.height {
        row = bounds.rows.saturating_sub(1);
        side = TerminalCellSide::Right;
    } else {
        row = row.min(bounds.rows.saturating_sub(1));
    }

    Some(TerminalSelectionEndpoint {
        position: TerminalGridPosition { row, column },
        side,
    })
}

fn translate_key_event(event: &KeyDownEvent, context: TerminalBindingContext) -> TerminalKeyAction {
    if let Some(action) = terminal_binding_action(&event.keystroke, context) {
        return action;
    }

    TerminalKeyAction::SendInput(TerminalInputAction::SendKeystroke(event.keystroke.clone()))
}

// Keep terminal-specific raw text overrides separate from keystroke translation,
// following the same SendText/SendKeystroke split used by the terminal layer.
fn terminal_binding_action(
    keystroke: &Keystroke,
    context: TerminalBindingContext,
) -> Option<TerminalKeyAction> {
    let key = keystroke.key.to_ascii_lowercase();

    for binding in terminal_binding_specs() {
        if binding.key != key {
            continue;
        }
        if binding
            .selection
            .is_some_and(|required| required != context.has_selection)
        {
            continue;
        }
        if binding
            .bracketed_paste
            .is_some_and(|required| required != context.bracketed_paste)
        {
            continue;
        }
        if !binding.shortcut.matches(keystroke.modifiers) {
            continue;
        }

        return Some(match binding.action {
            TerminalBindingOutcome::CloseSession => TerminalKeyAction::CloseSession,
            TerminalBindingOutcome::CopySelection => TerminalKeyAction::CopySelection,
            TerminalBindingOutcome::Paste => TerminalKeyAction::Paste,
            TerminalBindingOutcome::SendText(text) => {
                TerminalKeyAction::SendInput(TerminalInputAction::SendText(text.to_string()))
            }
        });
    }

    None
}

fn resolve_terminal_input_text(
    input: &TerminalInputAction,
    context: TerminalInputContext,
) -> Option<String> {
    match input {
        TerminalInputAction::SendText(text) => Some(text.clone()),
        TerminalInputAction::SendKeystroke(keystroke) => {
            translate_terminal_keystroke(keystroke, context)
        }
    }
}

fn terminal_clipboard_payload(clipboard: &ClipboardItem) -> Option<TerminalClipboardPayload> {
    match clipboard.entries().first() {
        Some(ClipboardEntry::Image(image)) if !image.bytes.is_empty() => {
            Some(TerminalClipboardPayload::RawBytes(vec![0x16]))
        }
        _ => clipboard.text().map(TerminalClipboardPayload::Text),
    }
}

fn translate_terminal_keystroke(
    keystroke: &Keystroke,
    context: TerminalInputContext,
) -> Option<String> {
    let key = keystroke.key.to_ascii_lowercase();
    let modifiers = keystroke.modifiers;
    let secondary = modifiers.control || modifiers.platform;
    let alt_as_meta = terminal_option_as_meta_enabled(context.option_as_meta);
    let alt_modifier = modifiers.alt && alt_as_meta;

    if modifiers.control && !modifiers.alt && !modifiers.platform {
        if let Some(control_char) = control_character(&key) {
            return Some(control_char.to_string());
        }
        if let Some(control_sequence) = control_symbol_sequence(&key) {
            return Some(control_sequence);
        }
    }

    if let Some(sequence) =
        special_key_sequence(&key, modifiers.shift, alt_modifier, secondary, context.mode)
    {
        return Some(sequence);
    }

    if key == "space" {
        if modifiers.control && !modifiers.alt && !modifiers.platform {
            return Some("\u{0}".to_string());
        }
        if alt_modifier && !secondary {
            return Some("\u{1b} ".to_string());
        }
        if !secondary && !alt_modifier {
            return Some(" ".to_string());
        }
    }

    if let Some(text) = keystroke.key_char.clone() {
        if alt_modifier && !secondary {
            return Some(format!("\u{1b}{text}"));
        }
        if !secondary || modifiers.shift {
            return Some(text);
        }
    }

    None
}

fn terminal_binding_specs() -> &'static [TerminalBindingSpec] {
    &[
        TerminalBindingSpec {
            key: "w",
            shortcut: TerminalShortcut::Secondary { shift: true },
            selection: None,
            bracketed_paste: None,
            action: TerminalBindingOutcome::CloseSession,
        },
        TerminalBindingSpec {
            key: "c",
            shortcut: TerminalShortcut::Secondary { shift: true },
            selection: None,
            bracketed_paste: None,
            action: TerminalBindingOutcome::CopySelection,
        },
        TerminalBindingSpec {
            key: "c",
            shortcut: TerminalShortcut::Secondary { shift: false },
            selection: Some(true),
            bracketed_paste: None,
            action: TerminalBindingOutcome::CopySelection,
        },
        TerminalBindingSpec {
            key: "v",
            shortcut: TerminalShortcut::Secondary { shift: false },
            selection: None,
            bracketed_paste: None,
            action: TerminalBindingOutcome::Paste,
        },
        TerminalBindingSpec {
            key: "enter",
            shortcut: TerminalShortcut::Control { shift: false },
            selection: None,
            bracketed_paste: None,
            action: TerminalBindingOutcome::SendText("\n"),
        },
        TerminalBindingSpec {
            key: "enter",
            shortcut: TerminalShortcut::ShiftOnly,
            selection: None,
            bracketed_paste: None,
            action: TerminalBindingOutcome::SendText("\n"),
        },
    ]
}

impl TerminalShortcut {
    fn matches(self, modifiers: Modifiers) -> bool {
        match self {
            Self::Secondary { shift } => {
                (modifiers.control || modifiers.platform)
                    && modifiers.shift == shift
                    && !modifiers.alt
            }
            Self::Control { shift } => {
                modifiers.control
                    && !modifiers.platform
                    && modifiers.shift == shift
                    && !modifiers.alt
            }
            Self::ShiftOnly => {
                modifiers.shift && !modifiers.control && !modifiers.platform && !modifiers.alt
            }
        }
    }
}

fn terminal_option_as_meta_enabled(option_as_meta: bool) -> bool {
    !cfg!(target_os = "macos") || option_as_meta
}

fn control_character(key: &str) -> Option<char> {
    let byte = key.as_bytes().first().copied()?;
    if byte.is_ascii_alphabetic() {
        Some((byte.to_ascii_lowercase() & 0x1f) as char)
    } else {
        None
    }
}

fn control_symbol_sequence(key: &str) -> Option<String> {
    match key {
        "2" | "@" => Some("\u{0}".to_string()),
        "[" => Some("\u{1b}".to_string()),
        "\\" => Some("\u{1c}".to_string()),
        "]" => Some("\u{1d}".to_string()),
        "6" | "^" => Some("\u{1e}".to_string()),
        "-" | "_" => Some("\u{1f}".to_string()),
        "/" | "?" => Some("\u{7f}".to_string()),
        _ => None,
    }
}

fn special_key_sequence(
    key: &str,
    shift: bool,
    alt: bool,
    secondary: bool,
    mode: crate::terminal::session::TerminalModeSnapshot,
) -> Option<String> {
    let modifier = modifier_parameter(shift, alt, secondary);
    match key {
        "enter" => Some("\r".to_string()),
        "tab" if shift => Some("\u{1b}[Z".to_string()),
        "tab" => Some("\t".to_string()),
        "backspace" if alt && !secondary => Some("\u{1b}\u{7f}".to_string()),
        "backspace" => Some("\u{7f}".to_string()),
        "escape" => Some("\u{1b}".to_string()),
        "up" => Some(cursor_sequence('A', modifier, mode.app_cursor)),
        "down" => Some(cursor_sequence('B', modifier, mode.app_cursor)),
        "right" => Some(cursor_sequence('C', modifier, mode.app_cursor)),
        "left" => Some(cursor_sequence('D', modifier, mode.app_cursor)),
        "home" => Some(home_end_sequence('H', modifier, mode.app_cursor)),
        "end" => Some(home_end_sequence('F', modifier, mode.app_cursor)),
        "pageup" => Some(csi_tilde_sequence(5, modifier)),
        "pagedown" => Some(csi_tilde_sequence(6, modifier)),
        "insert" => Some(csi_tilde_sequence(2, modifier)),
        "delete" => Some(csi_tilde_sequence(3, modifier)),
        "f1" => Some(function_sequence('P', 11, modifier)),
        "f2" => Some(function_sequence('Q', 12, modifier)),
        "f3" => Some(function_sequence('R', 13, modifier)),
        "f4" => Some(function_sequence('S', 14, modifier)),
        "f5" => Some(csi_tilde_sequence(15, modifier)),
        "f6" => Some(csi_tilde_sequence(17, modifier)),
        "f7" => Some(csi_tilde_sequence(18, modifier)),
        "f8" => Some(csi_tilde_sequence(19, modifier)),
        "f9" => Some(csi_tilde_sequence(20, modifier)),
        "f10" => Some(csi_tilde_sequence(21, modifier)),
        "f11" => Some(csi_tilde_sequence(23, modifier)),
        "f12" => Some(csi_tilde_sequence(24, modifier)),
        _ => None,
    }
}

fn modifier_parameter(shift: bool, alt: bool, secondary: bool) -> Option<u8> {
    let mut value = 1;
    if shift {
        value += 1;
    }
    if alt {
        value += 2;
    }
    if secondary {
        value += 4;
    }
    (value > 1).then_some(value)
}

fn cursor_sequence(suffix: char, modifier: Option<u8>, app_cursor: bool) -> String {
    match modifier {
        Some(modifier) => format!("\u{1b}[1;{modifier}{suffix}"),
        None if app_cursor => format!("\u{1b}O{suffix}"),
        None => format!("\u{1b}[{suffix}"),
    }
}

fn home_end_sequence(suffix: char, modifier: Option<u8>, app_cursor: bool) -> String {
    match modifier {
        Some(modifier) => format!("\u{1b}[1;{modifier}{suffix}"),
        None if app_cursor => format!("\u{1b}O{suffix}"),
        None => format!("\u{1b}[{suffix}"),
    }
}

fn csi_tilde_sequence(code: u8, modifier: Option<u8>) -> String {
    match modifier {
        Some(modifier) => format!("\u{1b}[{code};{modifier}~"),
        None => format!("\u{1b}[{code}~"),
    }
}

fn function_sequence(ss3_suffix: char, _csi_code: u8, modifier: Option<u8>) -> String {
    match modifier {
        Some(modifier) => format!("\u{1b}[1;{modifier}{ss3_suffix}"),
        None => format!("\u{1b}O{ss3_suffix}"),
    }
}

fn mouse_move_report(
    mode: crate::terminal::session::TerminalModeSnapshot,
    cell: TerminalGridPosition,
    button: Option<MouseButton>,
    modifiers: Modifiers,
) -> Option<Vec<u8>> {
    let button = TerminalMouseButton::from_move_button(button)?;
    if !(mode.mouse_drag || mode.mouse_motion) {
        return None;
    }
    if mode.mouse_drag && matches!(button, TerminalMouseButton::NoneMove) {
        return None;
    }

    mouse_report_bytes(
        cell,
        button as u8,
        true,
        modifiers,
        TerminalMouseFormat::from_mode(mode),
    )
}

fn mouse_button_report(
    mode: crate::terminal::session::TerminalModeSnapshot,
    cell: TerminalGridPosition,
    button: MouseButton,
    modifiers: Modifiers,
    pressed: bool,
) -> Option<Vec<u8>> {
    if !mode.mouse_reporting() {
        return None;
    }

    let button = TerminalMouseButton::from_button(button)?;
    mouse_report_bytes(
        cell,
        button as u8,
        pressed,
        modifiers,
        TerminalMouseFormat::from_mode(mode),
    )
}

fn mouse_scroll_report(
    mode: crate::terminal::session::TerminalModeSnapshot,
    cell: TerminalGridPosition,
    scroll_lines: i32,
    event: &ScrollWheelEvent,
) -> Option<Vec<Vec<u8>>> {
    if !mode.mouse_reporting() {
        return None;
    }

    let report = mouse_report_bytes(
        cell,
        TerminalMouseButton::from_scroll(event) as u8,
        true,
        event.modifiers,
        TerminalMouseFormat::from_mode(mode),
    )?;
    Some(
        std::iter::repeat(report)
            .take(scroll_lines.unsigned_abs() as usize)
            .collect(),
    )
}

fn mouse_report_bytes(
    cell: TerminalGridPosition,
    button: u8,
    pressed: bool,
    modifiers: Modifiers,
    format: TerminalMouseFormat,
) -> Option<Vec<u8>> {
    let mut modifier_bits = 0;
    if modifiers.shift {
        modifier_bits += 4;
    }
    if modifiers.alt {
        modifier_bits += 8;
    }
    if modifiers.control {
        modifier_bits += 16;
    }

    match format {
        TerminalMouseFormat::Sgr => Some(sgr_mouse_bytes(button + modifier_bits, cell, pressed)),
        TerminalMouseFormat::Normal { utf8 } => {
            let button = if pressed {
                button + modifier_bits
            } else {
                3 + modifier_bits
            };
            normal_mouse_bytes(cell, button, utf8)
        }
    }
}

fn sgr_mouse_bytes(button: u8, cell: TerminalGridPosition, pressed: bool) -> Vec<u8> {
    let terminator = if pressed { 'M' } else { 'm' };
    format!(
        "\u{1b}[<{};{};{}{}",
        button,
        cell.column + 1,
        cell.row + 1,
        terminator
    )
    .into_bytes()
}

fn normal_mouse_bytes(cell: TerminalGridPosition, button: u8, utf8: bool) -> Option<Vec<u8>> {
    let max_point = if utf8 { 2015 } else { 223 };
    if cell.row >= max_point || cell.column >= max_point {
        return None;
    }

    let mut message = vec![b'\x1b', b'[', b'M', 32 + button];
    let encode_position = |position: usize| -> Vec<u8> {
        let position = 32 + 1 + position;
        let first = 0xC0 + position / 64;
        let second = 0x80 + (position & 63);
        vec![first as u8, second as u8]
    };

    if utf8 && cell.column >= 95 {
        message.extend(encode_position(cell.column));
    } else {
        message.push(32 + 1 + cell.column as u8);
    }

    if utf8 && cell.row >= 95 {
        message.extend(encode_position(cell.row));
    } else {
        message.push(32 + 1 + cell.row as u8);
    }

    Some(message)
}

fn alt_scroll_bytes(scroll_lines: i32) -> Vec<u8> {
    let command = if scroll_lines < 0 { b'A' } else { b'B' };
    let mut content = Vec::with_capacity(scroll_lines.unsigned_abs() as usize * 3);
    for _ in 0..scroll_lines.unsigned_abs() {
        content.push(0x1b);
        content.push(b'O');
        content.push(command);
    }
    content
}

fn server_port_refresh_interval(
    _runtime: &RuntimeState,
    _state: &AppState,
    _snapshot: &ServerPortSnapshotState,
) -> std::time::Duration {
    std::time::Duration::from_secs(1)
}

fn live_server_ports(state: &AppState, runtime: &RuntimeState) -> Vec<u16> {
    let mut ports = Vec::new();
    for project in state.projects() {
        for folder in &project.folders {
            for command in &folder.commands {
                let Some(port) = command.port else {
                    continue;
                };
                let Some(session) = runtime.sessions.get(&command.id) else {
                    continue;
                };
                if session.status.is_live() {
                    ports.push(port);
                }
            }
        }
    }

    ports.sort_unstable();
    ports.dedup();
    ports
}

fn tracked_server_ports(state: &AppState) -> Vec<u16> {
    let mut ports = Vec::new();
    for project in state.projects() {
        for folder in &project.folders {
            for command in &folder.commands {
                if let Some(port) = command.port {
                    ports.push(port);
                }
            }
        }
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn remote_forwardable_ports(snapshot: &remote::RemoteWorkspaceSnapshot) -> Vec<u16> {
    let mut ports = Vec::new();
    for project in snapshot.app_state.projects() {
        for folder in &project.folders {
            for command in &folder.commands {
                let Some(port) = command.port else {
                    continue;
                };
                let Some(session) = snapshot.runtime_state.sessions.get(&command.id) else {
                    continue;
                };
                let Some(status) = snapshot.port_statuses.get(&port) else {
                    continue;
                };
                if session.status.is_live() && status.in_use && runtime_owns_port(session, status) {
                    ports.push(port);
                }
            }
        }
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn remote_port_forward_rows(
    snapshot: &remote::RemoteWorkspaceSnapshot,
    states: &HashMap<u16, RemotePortForwardState>,
) -> Vec<RemotePortForwardDraft> {
    let live_ports = remote_forwardable_ports(snapshot)
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    tracked_server_ports(&snapshot.app_state)
        .into_iter()
        .map(|port| {
            let label = format!("localhost:{port}");
            match states.get(&port) {
                Some(state) if state.listener_active => RemotePortForwardDraft {
                    label,
                    status: "Forwarded".to_string(),
                    detail: state
                        .message
                        .clone()
                        .or_else(|| Some("Open URL uses this local mirror.".to_string())),
                    is_error: false,
                },
                Some(state) if state.local_port_busy => RemotePortForwardDraft {
                    label,
                    status: "Local port busy".to_string(),
                    detail: state.message.clone(),
                    is_error: true,
                },
                Some(state) => RemotePortForwardDraft {
                    label,
                    status: if live_ports.contains(&port) {
                        "Forward unavailable".to_string()
                    } else {
                        "Host server not live".to_string()
                    },
                    detail: state.message.clone(),
                    is_error: live_ports.contains(&port),
                },
                None if live_ports.contains(&port) => RemotePortForwardDraft {
                    label,
                    status: "Preparing forward".to_string(),
                    detail: Some("DevManager is setting up a local localhost mirror.".to_string()),
                    is_error: false,
                },
                None => RemotePortForwardDraft {
                    label,
                    status: "Host server not live".to_string(),
                    detail: Some(
                        "The host is not currently serving this tracked port.".to_string(),
                    ),
                    is_error: false,
                },
            }
        })
        .collect()
}

fn derive_server_indicator_states(
    state: &AppState,
    runtime: &RuntimeState,
    port_statuses: &HashMap<u16, PortStatus>,
) -> HashMap<String, sidebar::ServerIndicatorState> {
    let mut indicators = HashMap::new();
    for project in state.projects() {
        for folder in &project.folders {
            for command in &folder.commands {
                let session = runtime.sessions.get(&command.id);
                indicators.insert(
                    command.id.clone(),
                    derive_server_indicator(session, command.port, port_statuses),
                );
            }
        }
    }
    indicators
}

fn derive_server_indicator(
    session: Option<&SessionRuntimeState>,
    port: Option<u16>,
    port_statuses: &HashMap<u16, PortStatus>,
) -> sidebar::ServerIndicatorState {
    let Some(session) = session else {
        return sidebar::ServerIndicatorState::Stopped;
    };

    match session.status {
        SessionStatus::Stopped => sidebar::ServerIndicatorState::Stopped,
        SessionStatus::Starting => sidebar::ServerIndicatorState::Unready,
        SessionStatus::Running => match port {
            Some(port) => {
                if port_statuses
                    .get(&port)
                    .is_some_and(|status| status.in_use && runtime_owns_port(session, status))
                {
                    sidebar::ServerIndicatorState::Ready
                } else {
                    sidebar::ServerIndicatorState::Unready
                }
            }
            None => sidebar::ServerIndicatorState::Ready,
        },
        SessionStatus::Stopping => sidebar::ServerIndicatorState::Stopping,
        SessionStatus::Crashed => sidebar::ServerIndicatorState::Crashed,
        SessionStatus::Exited => sidebar::ServerIndicatorState::Exited,
        SessionStatus::Failed => sidebar::ServerIndicatorState::Failed,
    }
}

fn is_managed_port_owner(
    active_session: Option<&crate::terminal::session::TerminalSessionView>,
    status: &PortStatus,
) -> bool {
    active_session
        .map(|session| runtime_owns_port(&session.runtime, status))
        .unwrap_or(false)
}

fn runtime_owns_port(session: &SessionRuntimeState, status: &PortStatus) -> bool {
    let Some(pid) = status.pid else {
        return false;
    };

    if session.pid == Some(pid) {
        return true;
    }

    session.resources.process_ids.contains(&pid)
}

fn normalize_optional_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn remote_runtime_revision(runtime: &RuntimeState) -> u64 {
    let mut entries = runtime
        .sessions
        .iter()
        .map(|(session_id, session)| (session_id.clone(), session.dirty_generation))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    local_stable_hash(&(runtime.active_session_id.clone(), entries))
}

fn local_stable_hash<T: serde::Serialize>(value: &T) -> u64 {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&bytes, &mut hasher);
    std::hash::Hasher::finish(&hasher)
}

fn app_window_title() -> String {
    crate::persistence::app_instance_label()
        .map(|label| format!("{APP_WINDOW_TITLE} [{label}]"))
        .unwrap_or_else(|| APP_WINDOW_TITLE.to_string())
}

fn current_window_title(state: &AppState, runtime: &crate::state::RuntimeState) -> String {
    let Some(tab) = state.active_tab() else {
        return app_window_title();
    };

    let segments = [
        window_title_project_name(tab, state),
        active_tab_live_title(tab, runtime)
            .or_else(|| Some(window_title_fallback_label(tab, state))),
        Some(app_window_title()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    dedupe_adjacent_segments(segments).join(WINDOW_TITLE_SEPARATOR)
}

fn remote_ai_tab_payload(
    state: &AppState,
    session_id: &str,
    session_view: Option<crate::terminal::session::TerminalSessionView>,
) -> Option<RemoteActionPayload> {
    let tab = state.find_ai_tab_by_session(session_id)?;
    Some(RemoteActionPayload::AiTab {
        tab_id: tab.id.clone(),
        project_id: tab.project_id.clone(),
        tab_type: tab.tab_type.clone(),
        session_id: session_id.to_string(),
        label: tab.label.clone(),
        session_view,
    })
}

fn window_title_project_name(tab: &SessionTab, state: &AppState) -> Option<String> {
    if matches!(tab.tab_type, TabType::Ssh) {
        Some("SSH".to_string())
    } else {
        state
            .find_project(&tab.project_id)
            .map(|project| project.name.clone())
    }
}

fn active_tab_live_title(tab: &SessionTab, runtime: &crate::state::RuntimeState) -> Option<String> {
    tab.pty_session_id
        .as_deref()
        .or(tab.command_id.as_deref())
        .and_then(|session_id| runtime.sessions.get(session_id))
        .and_then(|session| session.title.as_deref())
        .and_then(normalize_optional_string)
        .filter(|title| is_meaningful_title(title))
}

fn is_meaningful_title(title: &str) -> bool {
    let t = title.trim();
    if t.is_empty() {
        return false;
    }
    if t.contains("\\system32\\") || t.contains("/bin/") || t.contains("/usr/") {
        return false;
    }
    if t.ends_with(".exe") && (t.contains('\\') || t.contains('/')) {
        return false;
    }
    true
}

fn window_title_fallback_label(tab: &SessionTab, state: &AppState) -> String {
    match tab.tab_type {
        TabType::Server => server_window_fallback_label(tab, state),
        TabType::Claude => tab.label.clone().unwrap_or_else(|| "Claude".to_string()),
        TabType::Codex => tab.label.clone().unwrap_or_else(|| "Codex".to_string()),
        TabType::Ssh => tab
            .ssh_connection_id
            .as_deref()
            .and_then(|connection_id| state.find_ssh_connection(connection_id))
            .map(|connection| connection.label.clone())
            .or_else(|| tab.label.clone())
            .unwrap_or_else(|| "SSH".to_string()),
    }
}

fn server_window_fallback_label(tab: &SessionTab, state: &AppState) -> String {
    let Some(command_id) = tab.command_id.as_deref() else {
        return "Server".to_string();
    };
    let Some(lookup) = state.find_command(command_id) else {
        return command_id.to_string();
    };
    lookup.folder.name.clone()
}

fn dedupe_adjacent_segments(segments: Vec<String>) -> Vec<String> {
    segments
        .into_iter()
        .filter(|segment| !segment.is_empty())
        .fold(Vec::new(), |mut deduped, segment| {
            if deduped.last() != Some(&segment) {
                deduped.push(segment);
            }
            deduped
        })
}

fn is_startup_restorable_tab(tab: &SessionTab) -> bool {
    matches!(tab.tab_type, TabType::Server | TabType::Ssh)
}

fn fetch_splash_image() -> Option<Arc<RenderImage>> {
    let bytes = ureq::get("https://picsum.photos/1920/1080")
        .call()
        .ok()?
        .into_body()
        .read_to_vec()
        .ok()?;
    let format = image::guess_format(&bytes).ok()?;
    let mut rgba = image::load_from_memory_with_format(&bytes, format)
        .ok()?
        .into_rgba8();
    // GPUI expects BGRA pixel order.
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])))
}

fn retain_startup_restorable_tabs(
    open_tabs: &mut Vec<SessionTab>,
    active_tab_id: &mut Option<String>,
) {
    open_tabs.retain(is_startup_restorable_tab);
    if active_tab_id
        .as_ref()
        .is_none_or(|active| !open_tabs.iter().any(|tab| &tab.id == active))
    {
        *active_tab_id = open_tabs.first().map(|tab| tab.id.clone());
    }
}

fn persisted_session_state(state: &AppState) -> SessionState {
    let mut session = state.session_state();
    if state.settings().restore_session_on_start == Some(false) {
        session.open_tabs.clear();
        session.active_tab_id = None;
        session.sidebar_collapsed = false;
    } else {
        retain_startup_restorable_tabs(&mut session.open_tabs, &mut session.active_tab_id);
    }
    session
}

fn restore_saved_tabs(
    process_manager: &ProcessManager,
    state: &mut AppState,
    _dimensions: SessionDimensions,
) -> Option<String> {
    retain_startup_restorable_tabs(&mut state.open_tabs, &mut state.active_tab_id);
    let recovered = process_manager.reconcile_saved_server_tabs(state);
    let ssh_restore = process_manager.restore_ssh_tabs(state);
    let mut restore_notes = Vec::new();
    if recovered > 0 {
        restore_notes.push(format!("recovered {recovered} server tab(s)"));
    }
    if ssh_restore.reattached > 0 || ssh_restore.recovered > 0 {
        restore_notes.push(format!(
            "re-attached {} SSH tab(s)",
            ssh_restore.reattached + ssh_restore.recovered
        ));
    }
    (!restore_notes.is_empty()).then(|| restore_notes.join(", "))
}

fn validate_terminal_font_size(value: Option<u16>) -> Result<Option<u16>, String> {
    match value {
        Some(value) if !(8..=24).contains(&value) => {
            Err("Terminal font size must be between 8 and 24.".to_string())
        }
        _ => Ok(value),
    }
}

fn validate_log_buffer_size(value: Option<u32>) -> Result<u32, String> {
    let value = value.unwrap_or(10_000);
    if (100..=100_000).contains(&value) {
        Ok(value)
    } else {
        Err("Log buffer size must be between 100 and 100,000.".to_string())
    }
}

fn external_terminal_shell_path(settings: &crate::models::Settings) -> Option<String> {
    if cfg!(target_os = "macos") {
        let shell = match settings.mac_terminal_profile.clone().unwrap_or_default() {
            MacTerminalProfile::System => {
                std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string())
            }
            MacTerminalProfile::Zsh => "/bin/zsh".to_string(),
            MacTerminalProfile::Bash => "/bin/bash".to_string(),
        };
        Some(shell)
    } else {
        None
    }
}

fn parse_optional_u16(value: &str) -> Result<Option<u16>, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    trimmed
        .parse::<u16>()
        .map(Some)
        .map_err(|_| format!("`{trimmed}` is not a valid number"))
}

fn parse_optional_u32(value: &str) -> Result<Option<u32>, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    trimmed
        .parse::<u32>()
        .map(Some)
        .map_err(|_| format!("`{trimmed}` is not a valid number"))
}

fn parse_args_text(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.to_string())
        .collect()
}

fn parse_env_text(value: &str) -> Option<HashMap<String, String>> {
    let mut env = HashMap::new();

    for segment in value.lines().flat_map(|line| line.split(';')) {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            continue;
        }
        env.insert(key.to_string(), value.to_string());
    }

    (!env.is_empty()).then_some(env)
}

fn format_env_pairs(env: Option<&HashMap<String, String>>) -> String {
    env.map(|env| {
        env.iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(";")
    })
    .unwrap_or_default()
}

fn current_timestamp_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

fn next_entity_id(prefix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let counter = EDITOR_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{millis:x}-{counter:x}")
}

fn build_project_folders_from_selection(
    scan_entries: &[crate::models::RootScanEntry],
    selected_folder_paths: &std::collections::BTreeSet<String>,
    selected_scripts: &HashMap<String, std::collections::BTreeSet<String>>,
    selected_port_variables: &HashMap<String, Option<String>>,
) -> Vec<ProjectFolder> {
    scan_entries
        .iter()
        .map(|entry| {
            let selected_names = selected_scripts
                .get(&entry.path)
                .cloned()
                .unwrap_or_default();
            let selected_port_variable =
                selected_port_variables.get(&entry.path).cloned().flatten();
            let selected_port =
                scanner_service::port_for_variable(&entry.ports, selected_port_variable.as_deref());
            let commands = entry
                .scripts
                .iter()
                .filter(|script| selected_names.contains(&script.name))
                .map(|script| {
                    scanner_service::build_run_command_from_scanned_script(
                        script,
                        next_entity_id("command"),
                        selected_port,
                    )
                })
                .collect();

            ProjectFolder {
                id: next_entity_id("folder"),
                name: entry.name.clone(),
                folder_path: entry.path.clone(),
                commands,
                env_file_path: scanner_service::default_env_file_for_dir(std::path::Path::new(
                    &entry.path,
                )),
                port_variable: selected_port_variable,
                hidden: Some(!selected_folder_paths.contains(&entry.path)),
            }
        })
        .collect()
}

fn build_project_from_draft(draft: &ProjectDraft, existing: Option<&Project>) -> Project {
    let timestamp = current_timestamp_string();

    Project {
        id: draft
            .existing_id
            .clone()
            .unwrap_or_else(|| next_entity_id("project")),
        name: draft.name.trim().to_string(),
        root_path: draft.root_path.trim().to_string(),
        folders: existing
            .map(|project| project.folders.clone())
            .unwrap_or_default(),
        color: normalize_optional_string(&draft.color),
        pinned: Some(draft.pinned),
        notes: normalize_optional_string(&draft.notes),
        save_log_files: Some(draft.save_log_files),
        created_at: existing
            .map(|project| project.created_at.clone())
            .unwrap_or_else(|| timestamp.clone()),
        updated_at: timestamp,
    }
}

fn build_project_from_wizard(wizard: workspace::AddProjectWizard) -> Project {
    let workspace::AddProjectWizard {
        name,
        color,
        root_path,
        scan_entries,
        selected_folders,
        selected_scripts,
        selected_port_variables,
        ..
    } = wizard;

    let timestamp = current_timestamp_string();

    Project {
        id: next_entity_id("project"),
        name: if name.trim().is_empty() {
            "My App".to_string()
        } else {
            name.trim().to_string()
        },
        root_path: root_path.trim().to_string(),
        folders: build_project_folders_from_selection(
            &scan_entries,
            &selected_folders,
            &selected_scripts,
            &selected_port_variables,
        ),
        color: normalize_optional_string(&color),
        pinned: Some(false),
        notes: None,
        save_log_files: Some(true),
        created_at: timestamp.clone(),
        updated_at: timestamp,
    }
}

fn build_folder_commands_from_scan(
    draft: &FolderDraft,
    existing: Option<&ProjectFolder>,
) -> Vec<RunCommand> {
    let mut commands = existing
        .map(|folder| folder.commands.clone())
        .unwrap_or_default();
    let existing_labels: std::collections::BTreeSet<String> = commands
        .iter()
        .map(|command| command.label.clone())
        .collect();

    let Some(scan_result) = draft.scan_result.as_ref() else {
        return commands;
    };

    let selected_port = scanner_service::port_for_variable(
        &scan_result.ports,
        draft.selected_scanned_port_variable.as_deref().or_else(|| {
            (!draft.port_variable.trim().is_empty()).then_some(draft.port_variable.trim())
        }),
    );

    for selected_name in &draft.selected_scanned_scripts {
        if existing_labels.contains(selected_name) {
            continue;
        }
        let Some(script) = scan_result
            .scripts
            .iter()
            .find(|script| &script.name == selected_name)
        else {
            continue;
        };
        commands.push(scanner_service::build_run_command_from_scanned_script(
            script,
            next_entity_id("command"),
            selected_port,
        ));
    }

    commands
}

fn inspect_folder_runtime_metadata(
    folder_path: &str,
) -> (Option<String>, Option<DependencyStatus>) {
    if folder_path.trim().is_empty() {
        return (None, None);
    }

    let git_branch = scanner_service::read_git_branch(folder_path).ok().flatten();
    let dependency_status = scanner_service::check_dependencies(folder_path).ok();
    (git_branch, dependency_status)
}

fn load_folder_env_contents(folder_path: &str, env_file_path: &str) -> Option<String> {
    if folder_path.trim().is_empty() || env_file_path.trim().is_empty() {
        return None;
    }

    let path = std::path::Path::new(folder_path).join(env_file_path);
    if !path.exists() {
        return None;
    }

    env_service::read_env_text(&path).ok()
}

fn browse_remote_host_path(start_path: Option<&str>, directories_only: bool) -> Option<String> {
    let mut dialog = FileDialog::new();
    if let Some(path) = start_path.filter(|path| !path.trim().is_empty()) {
        dialog = dialog.set_directory(path);
    }
    let picked = if directories_only {
        dialog.pick_folder()
    } else {
        dialog.pick_file()
    };
    picked.map(|path| path.to_string_lossy().to_string())
}

fn list_remote_directory(path: &str) -> Result<Vec<remote::RemoteFsEntry>, String> {
    let dir = std::path::Path::new(path);
    let read_dir = std::fs::read_dir(dir)
        .map_err(|error| format!("Could not list `{path}` on the host: {error}"))?;
    let mut entries = Vec::new();
    for entry in read_dir {
        let entry =
            entry.map_err(|error| format!("Could not list `{path}` on the host: {error}"))?;
        let entry_path = entry.path();
        entries.push(remote_fs_entry_from_metadata(
            &entry_path,
            entry.metadata().ok(),
        ));
    }
    entries.sort_by(|left, right| {
        right.is_dir.cmp(&left.is_dir).then_with(|| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        })
    });
    Ok(entries)
}

fn remote_fs_entry_for_path(path: &str) -> Result<Option<remote::RemoteFsEntry>, String> {
    let target = std::path::Path::new(path);
    if !target.exists() {
        return Ok(None);
    }
    let metadata = std::fs::metadata(target)
        .map_err(|error| format!("Could not inspect `{path}` on the host: {error}"))?;
    Ok(Some(remote_fs_entry_from_metadata(target, Some(metadata))))
}

fn remote_fs_entry_from_metadata(
    path: &std::path::Path,
    metadata: Option<std::fs::Metadata>,
) -> remote::RemoteFsEntry {
    let modified_epoch_ms = metadata
        .as_ref()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(system_time_to_epoch_ms);
    remote::RemoteFsEntry {
        path: path.to_string_lossy().to_string(),
        name: path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string()),
        is_dir: metadata.as_ref().is_some_and(|metadata| metadata.is_dir()),
        size_bytes: metadata
            .as_ref()
            .and_then(|metadata| (!metadata.is_dir()).then_some(metadata.len())),
        modified_epoch_ms,
    }
}

fn collect_git_repositories(state: &AppState) -> Vec<RemoteGitRepo> {
    collect_git_repositories_from_projects(state.projects())
}

fn collect_git_repositories_from_projects(projects: &[Project]) -> Vec<RemoteGitRepo> {
    let mut repos = Vec::new();

    for project in projects {
        if git_service::is_repo(&project.root_path) {
            repos.push(RemoteGitRepo {
                label: project.name.clone(),
                path: project.root_path.clone(),
            });
        }

        for folder in &project.folders {
            if folder.folder_path.is_empty() || !git_service::is_repo(&folder.folder_path) {
                continue;
            }

            if repos.iter().any(|repo| repo.path == folder.folder_path) {
                continue;
            }

            repos.push(RemoteGitRepo {
                label: format!("{} / {}", project.name, folder.name),
                path: folder.folder_path.clone(),
            });
        }
    }

    repos
}

fn system_time_to_epoch_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

fn last_path_segment(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_string()
}

fn apply_root_scan_entries(
    wizard: &mut workspace::AddProjectWizard,
    scan_entries: Vec<crate::models::RootScanEntry>,
) {
    let selected_folders: std::collections::BTreeSet<String> = scan_entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    let count = scan_entries.len();
    wizard.scan_entries = scan_entries;
    wizard.selected_folders = selected_folders;
    wizard.scan_message = Some(if count == 0 {
        "No sub-folders with package.json or Cargo.toml were found.".to_string()
    } else {
        format!("Discovered {count} folder(s). Open Configure to review scripts and ports.")
    });
}

fn clear_root_scan_entries(wizard: &mut workspace::AddProjectWizard, message: String) {
    wizard.scan_entries.clear();
    wizard.selected_folders.clear();
    wizard.scan_message = Some(message);
}

fn write_terminal_export(kind: &str, text: &str) -> Result<std::path::PathBuf, String> {
    let file_name = format!(
        "devmanager-terminal-{kind}-{}.txt",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    );
    let path = std::env::temp_dir().join(file_name);
    std::fs::write(&path, text).map_err(|error| error.to_string())?;
    Ok(path)
}

fn selection_range(cursor: usize, anchor: Option<usize>) -> Option<(usize, usize)> {
    anchor.and_then(|a| {
        let (start, end) = if a < cursor { (a, cursor) } else { (cursor, a) };
        if start == end { None } else { Some((start, end)) }
    })
}

fn delete_selection(chars: &mut Vec<char>, cursor: &mut usize, anchor: &mut Option<usize>) {
    if let Some((start, end)) = selection_range(*cursor, *anchor) {
        chars.drain(start..end);
        *cursor = start;
        *anchor = None;
    }
}

fn apply_text_key_to_string(
    value: &mut String,
    cursor: &mut usize,
    selection_anchor: &mut Option<usize>,
    event: &KeyDownEvent,
    paste_text: Option<&str>,
    numeric_only: bool,
    allow_newlines: bool,
) -> bool {
    let key = event.keystroke.key.to_ascii_lowercase();
    let modifiers = event.keystroke.modifiers;
    let secondary = modifiers.control || modifiers.platform;
    let shift = modifiers.shift;
    let mut chars: Vec<char> = value.chars().collect();
    *cursor = (*cursor).min(chars.len());

    // Select all
    if secondary && key == "a" {
        *selection_anchor = Some(0);
        *cursor = chars.len();
        return true;
    }

    // Navigation keys with selection support
    match key.as_str() {
        "left" => {
            if shift {
                if selection_anchor.is_none() {
                    *selection_anchor = Some(*cursor);
                }
                *cursor = (*cursor).saturating_sub(1);
            } else if let Some(_) = selection_range(*cursor, *selection_anchor) {
                let start = (*cursor).min(selection_anchor.unwrap_or(*cursor));
                *cursor = start;
                *selection_anchor = None;
            } else {
                *cursor = (*cursor).saturating_sub(1);
                *selection_anchor = None;
            }
            return true;
        }
        "right" => {
            if shift {
                if selection_anchor.is_none() {
                    *selection_anchor = Some(*cursor);
                }
                *cursor = (*cursor + 1).min(chars.len());
            } else if let Some(_) = selection_range(*cursor, *selection_anchor) {
                let end = (*cursor).max(selection_anchor.unwrap_or(*cursor));
                *cursor = end;
                *selection_anchor = None;
            } else {
                *cursor = (*cursor + 1).min(chars.len());
                *selection_anchor = None;
            }
            return true;
        }
        "up" => {
            if !allow_newlines {
                return false;
            }
            if shift {
                if selection_anchor.is_none() {
                    *selection_anchor = Some(*cursor);
                }
            } else {
                *selection_anchor = None;
            }
            *cursor = move_cursor_vertically(value, *cursor, -1);
            return true;
        }
        "down" => {
            if !allow_newlines {
                return false;
            }
            if shift {
                if selection_anchor.is_none() {
                    *selection_anchor = Some(*cursor);
                }
            } else {
                *selection_anchor = None;
            }
            *cursor = move_cursor_vertically(value, *cursor, 1);
            return true;
        }
        "home" => {
            if shift {
                if selection_anchor.is_none() {
                    *selection_anchor = Some(*cursor);
                }
            } else {
                *selection_anchor = None;
            }
            *cursor = 0;
            return true;
        }
        "end" => {
            if shift {
                if selection_anchor.is_none() {
                    *selection_anchor = Some(*cursor);
                }
            } else {
                *selection_anchor = None;
            }
            *cursor = chars.len();
            return true;
        }
        _ => {}
    }

    // Editing keys — delete selection first if present
    match key.as_str() {
        "enter" => {
            if !allow_newlines {
                return false;
            }
            delete_selection(&mut chars, cursor, selection_anchor);
            chars.insert(*cursor, '\n');
            *cursor += 1;
            *value = chars.into_iter().collect();
            return true;
        }
        "backspace" => {
            if selection_range(*cursor, *selection_anchor).is_some() {
                delete_selection(&mut chars, cursor, selection_anchor);
                *value = chars.into_iter().collect();
                return true;
            }
            if *cursor > 0 {
                chars.remove(*cursor - 1);
                *cursor -= 1;
                *value = chars.into_iter().collect();
                return true;
            }
            return false;
        }
        "delete" => {
            if selection_range(*cursor, *selection_anchor).is_some() {
                delete_selection(&mut chars, cursor, selection_anchor);
                *value = chars.into_iter().collect();
                return true;
            }
            if *cursor < chars.len() {
                chars.remove(*cursor);
                *value = chars.into_iter().collect();
                return true;
            }
            return false;
        }
        "space" => {
            if numeric_only || secondary {
                return false;
            }
            delete_selection(&mut chars, cursor, selection_anchor);
            chars.insert(*cursor, ' ');
            *cursor += 1;
            *value = chars.into_iter().collect();
            return true;
        }
        _ => {}
    }

    if secondary {
        if let Some(paste_text) = paste_text {
            let filtered = filter_editor_text_input(paste_text, numeric_only, allow_newlines);
            if filtered.is_empty() {
                return false;
            }
            delete_selection(&mut chars, cursor, selection_anchor);
            let insertion: Vec<char> = filtered.chars().collect();
            chars.splice(*cursor..*cursor, insertion.clone());
            *cursor += insertion.len();
            *value = chars.into_iter().collect();
            return true;
        }
        return false;
    }

    if let Some(text) = event.keystroke.key_char.clone() {
        let filtered = filter_editor_text_input(&text, numeric_only, allow_newlines);
        if filtered.is_empty() {
            return false;
        }
        delete_selection(&mut chars, cursor, selection_anchor);
        let insertion: Vec<char> = filtered.chars().collect();
        chars.splice(*cursor..*cursor, insertion.clone());
        *cursor += insertion.len();
        *value = chars.into_iter().collect();
        return true;
    }

    false
}

fn filter_editor_text_input(value: &str, numeric_only: bool, allow_newlines: bool) -> String {
    if numeric_only {
        value
            .chars()
            .filter(|character| character.is_ascii_digit())
            .collect()
    } else {
        value
            .chars()
            .filter(|character| *character != '\r' && (allow_newlines || *character != '\n'))
            .collect()
    }
}

fn move_cursor_vertically(value: &str, cursor: usize, direction: isize) -> usize {
    let lines: Vec<&str> = value.split('\n').collect();
    if lines.is_empty() {
        return 0;
    }

    let mut line_index = 0usize;
    let mut remaining = cursor.min(value.chars().count());
    for (index, line) in lines.iter().enumerate() {
        let line_len = line.chars().count();
        if remaining <= line_len {
            line_index = index;
            break;
        }
        remaining = remaining.saturating_sub(line_len + 1);
    }

    let target_line_index = if direction.is_negative() {
        line_index.saturating_sub(direction.unsigned_abs())
    } else {
        (line_index + direction as usize).min(lines.len().saturating_sub(1))
    };

    if target_line_index == line_index {
        return cursor.min(value.chars().count());
    }

    let target_line_start = lines
        .iter()
        .take(target_line_index)
        .map(|line| line.chars().count() + 1)
        .sum::<usize>();
    let target_column = remaining.min(lines[target_line_index].chars().count());
    target_line_start + target_column
}

fn ssh_password_prompt(
    session: &crate::terminal::session::TerminalSessionView,
    connection: &SSHConnection,
) -> Option<SshPasswordPromptMatch> {
    if !matches!(session.runtime.session_kind, crate::state::SessionKind::Ssh) {
        return None;
    }

    let target = format!(
        "{}@{}",
        connection.username.trim().to_ascii_lowercase(),
        connection.host.trim().to_ascii_lowercase()
    );
    if target.is_empty() {
        return None;
    }

    let lines = visible_terminal_lines(&session.screen);
    let start = lines.len().saturating_sub(3);
    for index in start..lines.len() {
        let candidate = collapse_terminal_whitespace(&lines[index..].join(" "));
        let lower = candidate.to_ascii_lowercase();
        if lower.contains(&target) && lower.contains("password:") && lower.ends_with(':') {
            return Some(SshPasswordPromptMatch {
                fingerprint: candidate,
            });
        }
    }

    None
}

fn ssh_host_key_prompt(session: &crate::terminal::session::TerminalSessionView) -> bool {
    if !matches!(session.runtime.session_kind, crate::state::SessionKind::Ssh) {
        return false;
    }

    let lines = visible_terminal_lines(&session.screen);
    let start = lines.len().saturating_sub(3);
    for index in start..lines.len() {
        let candidate = collapse_terminal_whitespace(&lines[index..].join(" "));
        let lower = candidate.to_ascii_lowercase();
        if lower.contains("are you sure you want to continue connecting")
            && lower.contains("(yes/no")
            && lower.ends_with('?')
        {
            return true;
        }
    }

    false
}

fn visible_terminal_lines(
    screen: &crate::terminal::session::TerminalScreenSnapshot,
) -> Vec<String> {
    screen
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
        .filter(|line| !line.trim().is_empty())
        .collect()
}

fn collapse_terminal_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn scrollbar_thumb_top_ratio(display_offset: usize, max_offset: usize) -> f32 {
    if max_offset == 0 {
        1.0
    } else {
        1.0 - (display_offset as f32 / max_offset as f32)
    }
}

fn scrollbar_ratio_for_position(
    position: Point<Pixels>,
    geometry: TerminalScrollbarGeometry,
    grab_offset_px: f32,
) -> f32 {
    let thumb_range = (geometry.track_height - geometry.thumb_height).max(0.0);
    let position_y: f32 = position.y.into();
    let unclamped_thumb_top = position_y - geometry.track_top - grab_offset_px;

    if thumb_range <= f32::EPSILON {
        1.0
    } else {
        (unclamped_thumb_top / thumb_range).clamp(0.0, 1.0)
    }
}

fn display_offset_for_scrollbar_ratio(thumb_top_ratio: f32, max_offset: usize) -> usize {
    if max_offset == 0 {
        0
    } else {
        ((1.0 - thumb_top_ratio.clamp(0.0, 1.0)) * max_offset as f32).round() as usize
    }
}

fn terminal_view_needs_resize(
    last_dimensions: Option<SessionDimensions>,
    active_session: Option<&crate::terminal::session::TerminalSessionView>,
    dimensions: SessionDimensions,
) -> bool {
    last_dimensions != Some(dimensions)
        || active_session.map(|session| session.runtime.dimensions) != Some(dimensions)
}

fn remote_reconnect_backoff(attempts: u32) -> Duration {
    let multiplier = 1_u32.checked_shl(attempts.min(4)).unwrap_or(16);
    (REMOTE_RECONNECT_BASE_INTERVAL * multiplier).min(REMOTE_RECONNECT_MAX_INTERVAL)
}

fn fatal_remote_reconnect_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("protocol mismatch")
        || error.contains("saved remote credentials are no longer valid")
        || error.contains("pair with a host token")
        || error.contains("fingerprint")
        || error.contains("different host identity")
}

fn config_has_project(config: &AppConfig, project_id: &str) -> bool {
    config
        .projects
        .iter()
        .any(|project| project.id == project_id)
}

fn config_has_command(config: &AppConfig, command_id: &str) -> bool {
    config.projects.iter().any(|project| {
        project.folders.iter().any(|folder| {
            folder
                .commands
                .iter()
                .any(|command| command.id == command_id)
        })
    })
}

fn config_has_ssh_connection(config: &AppConfig, connection_id: &str) -> bool {
    config
        .ssh_connections
        .iter()
        .any(|connection| connection.id == connection_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        Project, ProjectFolder, RootScanEntry, RunCommand, SSHConnection, ScannedPort,
        ScannedScript, SessionTab, Settings,
    };
    use crate::services::ProcessManager;
    use crate::state::{RuntimeState, SessionRuntimeState};
    use crate::terminal::session::{
        TerminalBackend, TerminalCellSnapshot, TerminalScreenSnapshot, TerminalSessionView,
    };
    use gpui::point;
    use std::collections::{BTreeSet, HashMap};
    use std::path::PathBuf;

    fn sample_ai_tab() -> SessionTab {
        SessionTab {
            id: "tab-1".to_string(),
            tab_type: TabType::Claude,
            project_id: "project-1".to_string(),
            command_id: None,
            pty_session_id: Some("session-1".to_string()),
            label: Some("Claude 1".to_string()),
            ssh_connection_id: None,
        }
    }

    fn sample_server_tab() -> SessionTab {
        SessionTab {
            id: "server-tab".to_string(),
            tab_type: TabType::Server,
            project_id: "project-1".to_string(),
            command_id: Some("server-cmd".to_string()),
            pty_session_id: Some("server-cmd".to_string()),
            label: Some("Server".to_string()),
            ssh_connection_id: None,
        }
    }

    fn sample_ssh_tab() -> SessionTab {
        SessionTab {
            id: "ssh-tab".to_string(),
            tab_type: TabType::Ssh,
            project_id: "project-1".to_string(),
            command_id: None,
            pty_session_id: Some("ssh-session".to_string()),
            label: Some("SSH".to_string()),
            ssh_connection_id: Some("ssh-1".to_string()),
        }
    }

    fn sample_project() -> Project {
        Project {
            id: "project-1".to_string(),
            name: "Househunter".to_string(),
            root_path: ".".to_string(),
            folders: vec![
                ProjectFolder {
                    id: "folder-1".to_string(),
                    name: "api".to_string(),
                    folder_path: ".".to_string(),
                    commands: vec![RunCommand {
                        id: "server-cmd".to_string(),
                        label: "API Dev".to_string(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                ProjectFolder {
                    id: "folder-2".to_string(),
                    name: "web".to_string(),
                    folder_path: ".".to_string(),
                    commands: vec![RunCommand {
                        id: "web-cmd".to_string(),
                        label: "Web Dev".to_string(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn sample_ssh_connection(label: &str) -> SSHConnection {
        SSHConnection {
            id: "ssh-1".to_string(),
            label: label.to_string(),
            host: "example.com".to_string(),
            port: 22,
            username: "dev".to_string(),
            password: None,
        }
    }

    #[test]
    fn multiline_editor_cursor_moves_between_lines() {
        let value = "abc\nde\nfghi";

        assert_eq!(move_cursor_vertically(value, 2, 1), 6);
        assert_eq!(move_cursor_vertically(value, 6, 1), 9);
        assert_eq!(move_cursor_vertically(value, 9, -1), 6);
    }

    #[test]
    fn multiline_editor_cursor_clamps_to_shorter_lines() {
        let value = "abcdef\nxy\nmnop";

        assert_eq!(move_cursor_vertically(value, 5, 1), 9);
        assert_eq!(move_cursor_vertically(value, 9, 1), 12);
        assert_eq!(move_cursor_vertically(value, 12, -1), 9);
    }

    #[test]
    fn remote_ai_tab_payload_uses_session_lookup_metadata() {
        let mut state = AppState::default();
        state.config.projects.push(sample_project());
        state.open_ai_tab(
            "project-1",
            TabType::Claude,
            "claude-tab".to_string(),
            "claude-session".to_string(),
            Some("Claude 1".to_string()),
        );

        let payload = remote_ai_tab_payload(&state, "claude-session", None);

        match payload {
            Some(RemoteActionPayload::AiTab {
                tab_id,
                project_id,
                tab_type,
                session_id,
                label,
                session_view,
            }) => {
                assert_eq!(tab_id, "claude-tab");
                assert_eq!(project_id, "project-1");
                assert_eq!(tab_type, TabType::Claude);
                assert_eq!(session_id, "claude-session");
                assert_eq!(label.as_deref(), Some("Claude 1"));
                assert!(session_view.is_none());
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn scrollbar_ratio_maps_live_bottom_to_bottom_thumb() {
        assert_eq!(scrollbar_thumb_top_ratio(0, 120), 1.0);
        assert_eq!(scrollbar_thumb_top_ratio(120, 120), 0.0);
        assert_eq!(display_offset_for_scrollbar_ratio(1.0, 120), 0);
        assert_eq!(display_offset_for_scrollbar_ratio(0.0, 120), 120);
        assert_eq!(display_offset_for_scrollbar_ratio(0.5, 120), 60);
    }

    #[test]
    fn terminal_view_needs_resize_when_live_session_dimensions_do_not_match() {
        let expected = SessionDimensions {
            cols: 120,
            rows: 28,
            cell_width: 8,
            cell_height: 18,
        };
        let mut session = ssh_terminal_view(&["hello"]);
        session.runtime.dimensions = SessionDimensions {
            cols: 100,
            rows: 30,
            cell_width: 8,
            cell_height: 18,
        };

        assert!(terminal_view_needs_resize(
            Some(expected),
            Some(&session),
            expected
        ));
    }

    #[test]
    fn remote_reconnect_backoff_caps_at_max_interval() {
        assert_eq!(remote_reconnect_backoff(0), REMOTE_RECONNECT_BASE_INTERVAL);
        assert_eq!(
            remote_reconnect_backoff(1),
            REMOTE_RECONNECT_BASE_INTERVAL * 2
        );
        assert_eq!(remote_reconnect_backoff(8), REMOTE_RECONNECT_MAX_INTERVAL);
    }

    #[test]
    fn fatal_remote_reconnect_error_detects_unrecoverable_failures() {
        assert!(fatal_remote_reconnect_error(
            "Protocol mismatch. Host uses 4, client uses 3."
        ));
        assert!(fatal_remote_reconnect_error(
            "Saved remote credentials are no longer valid."
        ));
        assert!(!fatal_remote_reconnect_error(
            "Connect failed: Connection refused."
        ));
    }

    #[test]
    fn scrollbar_ratio_for_position_respects_track_offset_and_grab_offset() {
        let geometry = TerminalScrollbarGeometry {
            left: 0.0,
            top: 0.0,
            width: 10.0,
            height: 100.0,
            track_top: 10.0,
            track_height: 80.0,
            thumb_top: 34.0,
            thumb_height: 20.0,
            max_offset: 120,
        };

        let ratio = scrollbar_ratio_for_position(point(px(5.0), px(44.0)), geometry, 10.0);
        assert!((ratio - 0.4).abs() < 0.001);
    }

    fn snapshot_cell(character: char) -> TerminalCellSnapshot {
        TerminalCellSnapshot {
            character,
            zero_width: Vec::new(),
            foreground: 0,
            background: 0,
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

    fn screen_from_lines(lines: &[&str]) -> TerminalScreenSnapshot {
        let rendered_lines: Vec<Vec<TerminalCellSnapshot>> = lines
            .iter()
            .map(|line| line.chars().map(snapshot_cell).collect())
            .collect();
        let cols = rendered_lines
            .iter()
            .map(|line| line.len())
            .max()
            .unwrap_or(0);
        TerminalScreenSnapshot {
            lines: rendered_lines,
            cols,
            rows: lines.len(),
            ..Default::default()
        }
    }

    fn ssh_terminal_view(lines: &[&str]) -> TerminalSessionView {
        let mut runtime = SessionRuntimeState::new(
            "ssh-session",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        runtime.session_kind = crate::state::SessionKind::Ssh;
        runtime.status = crate::state::SessionStatus::Running;
        runtime.ssh_launch = Some(crate::state::SshLaunchSpec {
            tab_id: "ssh-tab".to_string(),
            ssh_connection_id: "ssh-1".to_string(),
            project_id: "project-1".to_string(),
            cwd: PathBuf::from("."),
            program: "ssh".to_string(),
            args: Vec::new(),
        });

        TerminalSessionView {
            runtime,
            screen: screen_from_lines(lines),
        }
    }

    #[test]
    fn ssh_password_prompt_matches_connection_target() {
        let connection = sample_ssh_connection("SSH");
        let session = ssh_terminal_view(&["dev@example.com's password:"]);

        let prompt = ssh_password_prompt(&session, &connection);

        assert!(prompt.is_some());
        assert_eq!(
            prompt.map(|prompt| prompt.fingerprint),
            Some("dev@example.com's password:".to_string())
        );
    }

    #[test]
    fn ssh_password_prompt_ignores_unrelated_password_prompts() {
        let connection = sample_ssh_connection("SSH");
        let session = ssh_terminal_view(&["[sudo] password for root:"]);

        assert!(ssh_password_prompt(&session, &connection).is_none());
    }

    #[test]
    fn ssh_password_prompt_matches_wrapped_login_prompt() {
        let connection = sample_ssh_connection("SSH");
        let session = ssh_terminal_view(&["dev@example.com's", "password:"]);

        assert!(ssh_password_prompt(&session, &connection).is_some());
    }

    #[test]
    fn ssh_host_key_prompt_matches_confirmation_prompt() {
        let session = ssh_terminal_view(&[
            "The authenticity of host 'example.com (192.168.0.11)' can't be established.",
            "ED25519 key fingerprint is SHA256:abc123.",
            "Are you sure you want to continue connecting (yes/no/[fingerprint])?",
        ]);

        assert!(ssh_host_key_prompt(&session));
    }

    #[test]
    fn ssh_host_key_prompt_ignores_unrelated_yes_no_prompt() {
        let session = ssh_terminal_view(&["Overwrite existing file? (yes/no)"]);

        assert!(!ssh_host_key_prompt(&session));
    }

    #[test]
    fn persisted_session_state_clears_restore_data_when_disabled() {
        let mut state = AppState::default();
        let mut settings = Settings::default();
        settings.restore_session_on_start = Some(false);
        state.update_settings(settings);
        state.open_tabs.push(sample_ai_tab());
        state.active_tab_id = Some("tab-1".to_string());
        state.sidebar_collapsed = true;

        let session = persisted_session_state(&state);

        assert!(session.open_tabs.is_empty());
        assert!(session.active_tab_id.is_none());
        assert!(!session.sidebar_collapsed);
    }

    #[test]
    fn persisted_session_state_drops_ai_tabs_and_repairs_active_tab() {
        let mut state = AppState::default();
        let mut settings = Settings::default();
        settings.restore_session_on_start = Some(true);
        state.update_settings(settings);
        state.open_tabs.push(sample_ai_tab());
        state.open_tabs.push(sample_server_tab());
        state.open_tabs.push(sample_ssh_tab());
        state.active_tab_id = Some("tab-1".to_string());
        state.sidebar_collapsed = true;

        let session = persisted_session_state(&state);

        assert_eq!(
            session
                .open_tabs
                .iter()
                .map(|tab| tab.id.as_str())
                .collect::<Vec<_>>(),
            vec!["server-tab", "ssh-tab"]
        );
        assert_eq!(session.active_tab_id.as_deref(), Some("server-tab"));
        assert!(session.sidebar_collapsed);
    }

    #[test]
    fn restore_saved_tabs_does_not_restart_saved_server_tabs() {
        let mut state = AppState::default();
        state.open_tabs.push(sample_server_tab());
        state.active_tab_id = Some("server-tab".to_string());

        let manager = ProcessManager::new();
        let notice = restore_saved_tabs(&manager, &mut state, SessionDimensions::default());

        assert!(notice.is_none());
        assert!(manager.runtime_state().sessions.is_empty());
        assert_eq!(state.open_tabs.len(), 1);
        assert_eq!(state.active_tab_id.as_deref(), Some("server-tab"));
    }

    #[test]
    fn restore_saved_tabs_does_not_show_notice_for_disconnected_saved_ssh_tabs() {
        let mut state = AppState::default();
        state.open_tabs.push(sample_ssh_tab());
        state.active_tab_id = Some("ssh-tab".to_string());

        let manager = ProcessManager::new();
        let notice = restore_saved_tabs(&manager, &mut state, SessionDimensions::default());

        assert!(notice.is_none());
        assert_eq!(state.open_tabs.len(), 1);
        assert_eq!(state.open_tabs[0].id, "ssh-tab");
        assert_eq!(state.open_tabs[0].pty_session_id, None);
        assert_eq!(state.active_tab_id.as_deref(), Some("ssh-tab"));
    }

    #[test]
    fn restore_saved_tabs_drops_ai_tabs_and_falls_back_to_fresh_shell() {
        let mut state = AppState::default();
        state.open_tabs.push(sample_ai_tab());
        state.active_tab_id = Some("tab-1".to_string());

        let manager = ProcessManager::new();
        let notice = restore_saved_tabs(&manager, &mut state, SessionDimensions::default());

        assert!(notice.is_none());
        assert!(manager.runtime_state().sessions.is_empty());
        assert!(state.open_tabs.is_empty());
        assert!(state.active_tab_id.is_none());

        let active_spec = state.active_terminal_spec();
        assert!(active_spec.session_id.starts_with("phase1-shell"));
    }

    #[test]
    fn restore_saved_tabs_reselects_surviving_server_when_ai_tab_was_active() {
        let mut state = AppState::default();
        state.open_tabs.push(sample_ai_tab());
        state.open_tabs.push(sample_server_tab());
        state.active_tab_id = Some("tab-1".to_string());

        let manager = ProcessManager::new();
        let notice = restore_saved_tabs(&manager, &mut state, SessionDimensions::default());

        assert!(notice.is_none());
        assert_eq!(state.open_tabs.len(), 1);
        assert_eq!(state.open_tabs[0].id, "server-tab");
        assert_eq!(state.active_tab_id.as_deref(), Some("server-tab"));
    }

    #[test]
    fn build_project_from_wizard_uses_selected_scan_configuration() {
        let folder_path = "C:/Code/personal/househunter/api".to_string();
        let selected_scripts =
            HashMap::from([(folder_path.clone(), BTreeSet::from(["dev".to_string()]))]);
        let selected_port_variables =
            HashMap::from([(folder_path.clone(), Some("PORT".to_string()))]);
        let wizard = workspace::AddProjectWizard {
            name: "Househunter".to_string(),
            color: "#6366f1".to_string(),
            root_path: "C:/Code/personal/househunter".to_string(),
            step: 2,
            scan_entries: vec![RootScanEntry {
                path: folder_path.clone(),
                name: "api".to_string(),
                has_env: true,
                project_type: "node".to_string(),
                scripts: vec![
                    ScannedScript {
                        name: "build".to_string(),
                        command: "tsc -p tsconfig.json".to_string(),
                    },
                    ScannedScript {
                        name: "dev".to_string(),
                        command: "tsx watch src/server.ts".to_string(),
                    },
                ],
                ports: vec![ScannedPort {
                    variable: "PORT".to_string(),
                    port: 4555,
                    source: ".env".to_string(),
                }],
            }],
            selected_folders: BTreeSet::from([folder_path.clone()]),
            selected_scripts,
            selected_port_variables,
            ..Default::default()
        };

        let project = build_project_from_wizard(wizard);

        assert_eq!(project.name, "Househunter");
        assert_eq!(project.root_path, "C:/Code/personal/househunter");
        assert_eq!(project.folders.len(), 1);

        let folder = &project.folders[0];
        assert_eq!(folder.name, "api");
        assert_eq!(folder.folder_path, folder_path);
        assert_eq!(folder.port_variable.as_deref(), Some("PORT"));
        assert_eq!(folder.hidden, Some(false));
        assert_eq!(folder.commands.len(), 1);
        assert_eq!(folder.commands[0].label, "dev");
        assert_eq!(folder.commands[0].command, "npm");
        assert_eq!(folder.commands[0].args, vec!["run", "dev"]);
        assert_eq!(folder.commands[0].port, Some(4555));
    }

    #[test]
    fn derive_server_indicator_uses_managed_port_ownership() {
        let mut state = AppState::default();
        let mut project = sample_project();
        project.folders[0].commands[0].port = Some(5174);
        state.config.projects.push(project);

        let mut runtime = RuntimeState::new(false);
        let mut session = SessionRuntimeState::new(
            "server-cmd",
            PathBuf::from("."),
            SessionDimensions::default(),
            crate::terminal::session::TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.pid = Some(42);
        session.status = SessionStatus::Running;
        runtime
            .sessions
            .insert("server-cmd".to_string(), session.clone());

        let mut port_statuses = HashMap::new();
        port_statuses.insert(
            5174,
            PortStatus {
                port: 5174,
                in_use: true,
                pid: Some(42),
                process_name: None,
            },
        );
        let indicators = derive_server_indicator_states(&state, &runtime, &port_statuses);
        assert_eq!(
            indicators.get("server-cmd"),
            Some(&sidebar::ServerIndicatorState::Ready)
        );

        port_statuses.insert(
            5174,
            PortStatus {
                port: 5174,
                in_use: true,
                pid: Some(99),
                process_name: None,
            },
        );
        let indicators = derive_server_indicator_states(&state, &runtime, &port_statuses);
        assert_eq!(
            indicators.get("server-cmd"),
            Some(&sidebar::ServerIndicatorState::Unready)
        );
    }

    #[test]
    fn derive_server_indicator_keeps_running_no_port_server_ready() {
        let mut session = SessionRuntimeState::new(
            "server-cmd",
            PathBuf::from("."),
            SessionDimensions::default(),
            crate::terminal::session::TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.status = SessionStatus::Running;

        assert_eq!(
            derive_server_indicator(Some(&session), None, &HashMap::new()),
            sidebar::ServerIndicatorState::Ready
        );
    }

    #[test]
    fn current_window_title_defaults_to_app_name_without_active_tab() {
        let state = AppState::default();
        let runtime = RuntimeState::new(false);

        assert_eq!(current_window_title(&state, &runtime), APP_WINDOW_TITLE);
    }

    #[test]
    fn current_window_title_prefers_live_terminal_title() {
        let mut state = AppState::default();
        state.config.projects.push(sample_project());
        state.open_tabs.push(sample_server_tab());
        state.active_tab_id = Some("server-tab".to_string());

        let mut runtime = RuntimeState::new(false);
        let mut session = SessionRuntimeState::new(
            "server-cmd",
            PathBuf::from("."),
            SessionDimensions::default(),
            crate::terminal::session::TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.note_title(Some("npm run dev".to_string()));
        runtime.sessions.insert("server-cmd".to_string(), session);

        assert_eq!(
            current_window_title(&state, &runtime),
            "Househunter • npm run dev • DevManager"
        );
    }

    #[test]
    fn current_window_title_uses_server_folder_and_command_label_fallback() {
        let mut state = AppState::default();
        state.config.projects.push(sample_project());
        state.open_tabs.push(sample_server_tab());
        state.active_tab_id = Some("server-tab".to_string());

        let runtime = RuntimeState::new(false);

        assert_eq!(
            current_window_title(&state, &runtime),
            "Househunter • api • DevManager"
        );
    }

    #[test]
    fn current_window_title_dedupes_adjacent_ssh_segments() {
        let mut state = AppState::default();
        state
            .config
            .ssh_connections
            .push(sample_ssh_connection("SSH"));
        state.open_tabs.push(sample_ssh_tab());
        state.active_tab_id = Some("ssh-tab".to_string());

        let runtime = RuntimeState::new(false);

        assert_eq!(current_window_title(&state, &runtime), "SSH • DevManager");
    }

    #[test]
    fn terminal_endpoint_uses_cell_half_and_clamps_to_edges() {
        let bounds = TerminalTextBounds {
            left: 0.0,
            top: 0.0,
            width: 40.0,
            height: 20.0,
            cell_width: 10.0,
            row_height: 10.0,
            rows: 2,
            cols: 4,
        };

        let left_half = terminal_endpoint_for_mouse(point(px(4.0), px(5.0)), bounds, true).unwrap();
        let right_half =
            terminal_endpoint_for_mouse(point(px(7.0), px(5.0)), bounds, true).unwrap();
        let edge = terminal_endpoint_for_mouse(point(px(40.0), px(19.0)), bounds, true).unwrap();

        assert_eq!(left_half.position.column, 0);
        assert_eq!(left_half.side, TerminalCellSide::Left);
        assert_eq!(right_half.position.column, 0);
        assert_eq!(right_half.side, TerminalCellSide::Right);
        assert_eq!(edge.position.column, 3);
        assert_eq!(edge.side, TerminalCellSide::Right);
    }

    #[test]
    fn buffer_line_for_viewport_row_accounts_for_display_offset() {
        let screen = TerminalScreenSnapshot {
            rows: 3,
            cols: 4,
            total_lines: 12,
            history_size: 9,
            display_offset: 2,
            ..Default::default()
        };

        assert_eq!(top_visible_buffer_line(&screen), 7);
        assert_eq!(
            buffer_line_for_viewport_row(&screen, screen.display_offset, 0),
            7
        );
        assert_eq!(
            buffer_line_for_viewport_row(&screen, screen.display_offset, 2),
            9
        );
        assert_eq!(buffer_line_for_viewport_row(&screen, 0, 0), 9);
    }

    #[test]
    fn semantic_selection_selects_whole_non_whitespace_run() {
        let line: Vec<TerminalCellSnapshot> = "cargo test".chars().map(snapshot_cell).collect();
        let screen = TerminalScreenSnapshot {
            lines: vec![line],
            cols: 10,
            rows: 1,
            ..Default::default()
        };

        let selection = terminal_selection_for_click(
            &screen,
            TerminalGridPosition { row: 0, column: 2 },
            TerminalSelectionMode::Semantic,
        )
        .unwrap();
        let snapshot = {
            let (start, end) = ordered_selection(selection.anchor, selection.head);
            view::TerminalSelectionSnapshot {
                start_row: start.position.row,
                start_column: boundary_column(start, screen.cols),
                end_row: end.position.row,
                end_column: boundary_column(end, screen.cols),
            }
        };

        assert_eq!(snapshot.start_column, 0);
        assert_eq!(snapshot.end_column, 5);
    }

    #[test]
    fn semantic_selection_keeps_buffer_row_when_scrolled_back() {
        let line: Vec<TerminalCellSnapshot> = "cargo test".chars().map(snapshot_cell).collect();
        let screen = TerminalScreenSnapshot {
            lines: vec![line],
            cols: 10,
            rows: 1,
            total_lines: 8,
            history_size: 7,
            display_offset: 3,
            ..Default::default()
        };

        let selection = terminal_selection_for_click(
            &screen,
            TerminalGridPosition { row: 4, column: 2 },
            TerminalSelectionMode::Semantic,
        )
        .unwrap();

        assert_eq!(selection.anchor.position.row, 4);
        assert_eq!(selection.head.position.row, 4);
    }

    #[test]
    fn sgr_mouse_reports_include_modifier_bits() {
        let mode = crate::terminal::session::TerminalModeSnapshot {
            mouse_report_click: true,
            sgr_mouse: true,
            ..Default::default()
        };
        let modifiers = Modifiers {
            shift: true,
            alt: true,
            ..Default::default()
        };

        let report = mouse_button_report(
            mode,
            TerminalGridPosition { row: 3, column: 4 },
            MouseButton::Left,
            modifiers,
            true,
        )
        .unwrap();

        assert_eq!(report, b"\x1b[<12;5;4M".to_vec());
    }

    #[test]
    fn alternate_scroll_uses_ss3_arrow_bytes() {
        assert_eq!(alt_scroll_bytes(-2), b"\x1bOA\x1bOA".to_vec());
        assert_eq!(alt_scroll_bytes(2), b"\x1bOB\x1bOB".to_vec());
    }

    fn key_down_event(source: &str) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke::parse(source).expect("valid keystroke"),
            is_held: false,
        }
    }

    #[test]
    fn ctrl_enter_binding_sends_raw_newline_text() {
        let action = translate_key_event(
            &key_down_event("ctrl-enter"),
            TerminalBindingContext::default(),
        );

        assert_eq!(
            action,
            TerminalKeyAction::SendInput(TerminalInputAction::SendText("\n".to_string()))
        );
    }

    #[test]
    fn plain_enter_uses_keystroke_path_and_stays_carriage_return() {
        let action =
            translate_key_event(&key_down_event("enter"), TerminalBindingContext::default());

        assert_eq!(
            action,
            TerminalKeyAction::SendInput(TerminalInputAction::SendKeystroke(
                Keystroke::parse("enter").expect("enter keystroke"),
            ))
        );
        assert_eq!(
            resolve_terminal_input_text(
                &TerminalInputAction::SendKeystroke(
                    Keystroke::parse("enter").expect("enter keystroke"),
                ),
                TerminalInputContext {
                    mode: Default::default(),
                    option_as_meta: false,
                },
            ),
            Some("\r".to_string())
        );
    }

    #[test]
    fn terminal_shortcuts_still_preserve_non_newline_actions() {
        assert_eq!(
            translate_key_event(&key_down_event("ctrl-v"), TerminalBindingContext::default()),
            TerminalKeyAction::Paste
        );
        assert_eq!(
            translate_key_event(
                &key_down_event("ctrl-shift-c"),
                TerminalBindingContext::default()
            ),
            TerminalKeyAction::CopySelection
        );
    }

    #[test]
    fn image_clipboard_payload_forwards_raw_ctrl_v() {
        let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, vec![1, 2, 3]);
        let clipboard = ClipboardItem::new_image(&image);

        assert_eq!(
            terminal_clipboard_payload(&clipboard),
            Some(TerminalClipboardPayload::RawBytes(vec![0x16]))
        );
    }

    #[test]
    fn shift_enter_binding_sends_raw_newline_text() {
        let action = translate_key_event(
            &key_down_event("shift-enter"),
            TerminalBindingContext::default(),
        );

        assert_eq!(
            action,
            TerminalKeyAction::SendInput(TerminalInputAction::SendText("\n".to_string()))
        );
    }

    #[test]
    fn ctrl_c_only_copies_when_selection_exists() {
        assert_eq!(
            translate_key_event(
                &key_down_event("ctrl-c"),
                TerminalBindingContext {
                    has_selection: true,
                    bracketed_paste: false,
                },
            ),
            TerminalKeyAction::CopySelection
        );
        assert_eq!(
            translate_key_event(&key_down_event("ctrl-c"), TerminalBindingContext::default()),
            TerminalKeyAction::SendInput(TerminalInputAction::SendKeystroke(
                Keystroke::parse("ctrl-c").expect("ctrl-c keystroke"),
            ))
        );
    }

    #[test]
    fn option_as_meta_matches_platform_behavior() {
        let input = TerminalInputAction::SendKeystroke(Keystroke {
            modifiers: Modifiers {
                alt: true,
                ..Default::default()
            },
            key: "a".to_string(),
            key_char: Some("a".to_string()),
        });

        assert_eq!(
            resolve_terminal_input_text(
                &input,
                TerminalInputContext {
                    mode: Default::default(),
                    option_as_meta: false,
                },
            ),
            Some(if cfg!(target_os = "macos") {
                "a".to_string()
            } else {
                "\u{1b}a".to_string()
            })
        );
    }
}
